use crate::{
    context::{RequestContext, RequestData},
    error::PointercrateError,
    middleware::auth::{AuthType, Authorization, Basic, Claims, Me, TAuthType},
    model::{demon::PartialDemon, user::PatchMe, Model, User},
    operation::{Delete, Get, Paginate, Paginator, Patch, Post},
    permissions::Permissions,
    view::demonlist::DemonlistOverview,
    Result,
};
use actix::{Actor, Addr, Handler, Message, SyncArbiter, SyncContext};
use diesel::{
    expression::{AsExpression, NonAggregate},
    pg::{Pg, PgConnection},
    query_builder::QueryFragment,
    r2d2::{ConnectionManager, Pool, PooledConnection},
    sql_types::{HasSqlType, NotNull, SqlOrd},
    AppearsOnTable, Connection, Expression, QueryDsl, QuerySource, RunQueryDsl,
    SelectableExpression,
};

use joinery::Joinable;
use log::{debug, info, trace, warn};
use std::{hash::Hash, marker::PhantomData};

/// Actor that executes database related actions on a thread pool
#[allow(missing_debug_implementations)]
pub struct DatabaseActor(pub Pool<ConnectionManager<PgConnection>>);

impl DatabaseActor {
    /// Initializes a [`DatabaseActor`] from environment data
    ///
    /// Attempts to connect to a PgSQL database via the URL found in the `DATABASE_URL` environment
    /// variable
    pub fn from_env() -> Addr<Self> {
        info!("Initializing pointercrate database connection pool");

        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is not set");
        let manager = ConnectionManager::<PgConnection>::new(database_url);
        let pool = Pool::builder()
            .build(manager)
            .expect("Failed to create database connection pool");

        SyncArbiter::start(4, move || DatabaseActor(pool.clone()))
    }

    /// Gets a connection from the connection pool
    fn connection(&self) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        self.0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)
    }

    /// Gets a connection that causes all audit entries generated by operations executed on it to be
    /// attributed to the given user
    fn audited_connection(
        &self, active_user: &User,
    ) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        let connection = self.connection()?;

        trace!(
            "Creating connection of which usage will be attributed to user {} in audit logs",
            active_user.id
        );

        diesel::sql_query("CREATE TEMPORARY TABLE IF NOT EXISTS active_user (id INTEGER);")
            .execute(&*connection)?;
        diesel::sql_query("DELETE FROM active_user").execute(&*connection)?;
        diesel::sql_query(format!(
            "INSERT INTO active_user VALUES({});",
            active_user.id
        ))
        .execute(&*connection)?;

        Ok(connection)
    }

    fn connection_for(
        &self, data: &RequestData,
    ) -> Result<PooledConnection<ConnectionManager<PgConnection>>> {
        match data {
            RequestData::External {
                user: Some(Me(ref user)),
                ..
            } => self.audited_connection(user),
            _ => self.connection(),
        }
    }
}

impl Actor for DatabaseActor {
    type Context = SyncContext<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        info!("Started pointercrate database actor! We can now interact with the database!")
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        info!(
            "Stopped pointercrate database actor! We can no longer interact with the database! :("
        )
    }
}

#[derive(Debug)]
pub struct GetDemonlistOverview;

impl Message for GetDemonlistOverview {
    type Result = Result<DemonlistOverview>;
}

impl Handler<GetDemonlistOverview> for DatabaseActor {
    type Result = Result<DemonlistOverview>;

    fn handle(&mut self, _: GetDemonlistOverview, _: &mut Self::Context) -> Self::Result {
        let connection = &*self.connection()?;
        let (admins, mods, helpers) = Get::get(
            (
                Permissions::ListAdministrator,
                Permissions::ListModerator,
                Permissions::ListHelper,
            ),
            RequestContext::Internal(connection),
        )?;
        let all_demons = PartialDemon::all()
            .order_by(crate::schema::demons::position)
            .load(connection)?;

        Ok(DemonlistOverview {
            demon_overview: all_demons,
            admins,
            mods,
            helpers,
        })
    }
}

#[derive(Debug)]
pub struct Auth<T: TAuthType>(pub Authorization, pub PhantomData<T>);

impl<T: TAuthType> Auth<T> {
    pub fn new(auth: Authorization) -> Self {
        Self(auth, PhantomData)
    }
}

impl<T: TAuthType> Message for Auth<T> {
    type Result = Result<Me>;
}

impl<T: TAuthType> Handler<Auth<T>> for DatabaseActor {
    type Result = Result<Me>;

    fn handle(&mut self, msg: Auth<T>, _: &mut Self::Context) -> Self::Result {
        info!("Received authorization request!");

        match T::auth_type() {
            AuthType::Basic => {
                info!("We are expected to perform basic authentication");

                if let Authorization::Basic { username, password } = msg.0 {
                    debug!("Trying to authorize user {}", username);
                    let connection = &*self.connection()?;

                    let user = User::get(username, RequestContext::Internal(connection))
                        .map_err(|_| PointercrateError::Unauthorized)?;

                    user.verify_password(&password).map(Me)
                } else {
                    warn!("No basic authentication found");

                    Err(PointercrateError::Unauthorized)
                }
            },
            AuthType::Token => {
                info!("We are expected to perform token authentication");

                if let Authorization::Token {
                    access_token,
                    csrf_token,
                } = msg.0
                {
                    // Well this is reassuring. Also we directly deconstruct it and only save the ID
                    // so we don't accidentally use unsafe values later on
                    let Claims { id, .. } =
                        jsonwebtoken::dangerous_unsafe_decode::<Claims>(&access_token)
                            .map_err(|_| PointercrateError::Unauthorized)?
                            .claims;

                    debug!(
                        "The token identified the user with id {}, validating...",
                        id
                    );

                    let connection = &*self.connection()?;

                    let user = User::get(id, RequestContext::Internal(connection))
                        .map_err(|_| PointercrateError::Unauthorized)?;

                    let user = user.validate_token(&access_token)?;

                    if let Some(ref csrf_token) = csrf_token {
                        user.validate_csrf_token(csrf_token)?
                    }

                    Ok(Me(user))
                } else {
                    warn!("No token authentication found");

                    Err(PointercrateError::Unauthorized)
                }
            },
        }
    }
}

/// Message that indicates the [`DatabaseActor`] to invalidate all access tokens to the account
/// authorized by the given [`Authorization`] object. The [`Authorization`] object must be of type
/// [`Authorization::Basic] for this.
///
/// Invalidation is done by re-randomizing the salt used for hashing the user's password (since the
/// key tokens are signed with contains the salt, this will invalidate all old access tokens).
///
/// ## Errors
/// + [`PointercrateError::Unauthorized`]: Authorization failed
#[derive(Debug)]
pub struct Invalidate(pub Authorization);

impl Message for Invalidate {
    type Result = Result<()>;
}

impl Handler<Invalidate> for DatabaseActor {
    type Result = Result<()>;

    fn handle(&mut self, msg: Invalidate, ctx: &mut Self::Context) -> Self::Result {
        if let Authorization::Basic { ref password, .. } = msg.0 {
            let password = password.clone();
            let user = self.handle(Auth::<Basic>(msg.0, PhantomData), ctx)?;
            let patch = PatchMe {
                password: Some(password),
                display_name: None,
                youtube_channel: None,
            };

            info!("Invalidating all access tokens for user {}", user.0.id);

            self.handle(
                PatchMessage::<Me, Me, _>::new(user, patch, RequestData::Internal),
                ctx,
            )
            .map(|_| ())
        } else {
            Err(PointercrateError::Unauthorized)
        }
    }
}

/*
/// Message that indicates the [`DatabaseActor`] to authorize a [`User`] by access token
///
/// ## Errors
/// + [`PointercrateError::Unauthorized`]: Authorization failed
#[derive(Debug)]
pub struct Token(pub Authorization);

/// Message that indicates the [`DatabaseActor`] to authorize a [`User`] using basic auth
///
/// ## Errors
/// + [`PointercrateError::Unauthorized`]: Authorization failed
#[derive(Debug)]
pub struct Basic(pub Authorization);

impl Message for Token {
    type Result = Result<User>;
}

// During authorization, all and every error that might come up will be converted into
// `PointercrateError::Unauthorized`
impl Handler<Token> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, msg: Token, _: &mut Self::Context) -> Self::Result {
        debug!("Attempting to perform token authorization (we're not logging the token for obvious reasons smh)");

        if let Authorization::Token {
            access_token,
            csrf_token,
        } = msg.0
        {
            // Well this is reassuring. Also we directly deconstruct it and only save the ID so we
            // don't accidentally use unsafe values later on
            let Claims { id, .. } = jsonwebtoken::dangerous_unsafe_decode::<Claims>(&access_token)
                .map_err(|_| PointercrateError::Unauthorized)?
                .claims;

            debug!(
                "The token identified the user with id {}, validating...",
                id
            );

            let user =
                User::get(id, &*self.connection()?).map_err(|_| PointercrateError::Unauthorized)?;

            let user = user.validate_token(&access_token)?;

            if let Some(ref csrf_token) = csrf_token {
                user.validate_csrf_token(csrf_token)?
            }

            Ok(user)
        } else {
            Err(PointercrateError::Unauthorized)
        }
    }
}

impl Message for Basic {
    type Result = Result<User>;
}

impl Handler<Basic> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, msg: Basic, _: &mut Self::Context) -> Self::Result {
        debug!("Attempting to perform basic authorization (we're not logging the password for even more obvious reasons smh)");

        if let Authorization::Basic { username, password } = msg.0 {
            debug!(
                "Trying to authorize user {} (still not logging the password)",
                username
            );

            let user = User::get(username, &*self.connection()?)
                .map_err(|_| PointercrateError::Unauthorized)?;

            user.verify_password(&password)
        } else {
            Err(PointercrateError::Unauthorized)
        }
    }
}*/

/// Message that requests the retrieval of an object of type `G` from the database using the
/// provided key. The user object (if provided) will be the user any generated audit log entries
/// will be attributed to
///
/// Calls [`Get::get`] with the provided key and a database connection from the internal connection
/// pool when handled
#[derive(Debug)]
pub struct GetMessage<Key, G: Get<Key>>(pub Key, pub RequestData, pub PhantomData<G>);

impl<Key, G: Get<Key>> GetMessage<Key, G> {
    pub fn new(key: Key, data: RequestData) -> Self {
        GetMessage(key, data, PhantomData)
    }
}

impl<Key, G: Get<Key> + 'static> Message for GetMessage<Key, G> {
    type Result = Result<G>;
}

impl<Key, G: Get<Key> + 'static> Handler<GetMessage<Key, G>> for DatabaseActor {
    type Result = Result<G>;

    fn handle(&mut self, msg: GetMessage<Key, G>, _: &mut Self::Context) -> Self::Result {
        let connection = &*self.connection_for(&msg.1)?;
        G::get(msg.0, msg.1.ctx(connection))
    }
}

/// Messages that requests the addition of a `P` object, generated from the given `T` data, to the
/// database. The user object (if provided) will be the user any generated audit log entries
/// will be attributed to
///
/// Calls [`Post::create_from`] with the provided [`PostData`] when handled
#[derive(Debug)]
pub struct PostMessage<T, P: Post<T> + 'static>(pub T, pub RequestData, pub PhantomData<P>);

impl<T, P: Post<T> + 'static> Message for PostMessage<T, P> {
    type Result = Result<P>;
}

impl<T, P: Post<T> + 'static> Handler<PostMessage<T, P>> for DatabaseActor {
    type Result = Result<P>;

    fn handle(&mut self, msg: PostMessage<T, P>, _: &mut Self::Context) -> Self::Result {
        let connection = &*self.connection_for(&msg.1)?;
        P::create_from(msg.0, msg.1.ctx(connection))
    }
}

/// Message that requests the deletion of an object of type `D`
///
/// Called [`Delete::delete`] when handled
#[derive(Debug)]
pub struct DeleteMessage<Key, D>(pub Key, pub RequestData, pub PhantomData<D>)
where
    D: Get<Key> + Delete + Hash;

impl<Key, D> DeleteMessage<Key, D>
where
    D: Get<Key> + Delete + Hash,
{
    pub fn new(key: Key, data: RequestData) -> Self {
        DeleteMessage(key, data, PhantomData)
    }
}

impl<Key, D> Message for DeleteMessage<Key, D>
where
    D: Get<Key> + Delete + Hash,
{
    type Result = Result<()>;
}

impl<Key, D> Handler<DeleteMessage<Key, D>> for DatabaseActor
where
    D: Get<Key> + Delete + Hash,
{
    type Result = Result<()>;

    fn handle(
        &mut self, DeleteMessage(key, data, _): DeleteMessage<Key, D>, _: &mut Self::Context,
    ) -> Self::Result {
        let connection = &*self.connection_for(&data)?;
        let ctx = data.ctx(connection);

        connection.transaction(|| D::get(key, ctx)?.delete(ctx))
    }
}

#[derive(Debug)]
pub struct PatchMessage<Key, P, H>(Key, H, RequestData, PhantomData<P>)
where
    P: Get<Key> + Patch<H> + Hash;

impl<Key, P, H> PatchMessage<Key, P, H>
where
    P: Get<Key> + Patch<H> + Hash,
{
    pub fn new(key: Key, fix: H, request_data: RequestData) -> Self {
        PatchMessage(key, fix, request_data, PhantomData)
    }
}

impl<Key, P, H> Message for PatchMessage<Key, P, H>
where
    P: Get<Key> + Patch<H> + Hash + 'static,
{
    type Result = Result<P>;
}

impl<Key, P, H> Handler<PatchMessage<Key, P, H>> for DatabaseActor
where
    P: Get<Key> + Patch<H> + Hash + 'static,
{
    type Result = Result<P>;

    fn handle(
        &mut self, PatchMessage(key, patch_data, request_data, _): PatchMessage<Key, P, H>,
        _: &mut Self::Context,
    ) -> Self::Result {
        let connection = &*self.connection_for(&request_data)?;
        let ctx = request_data.ctx(connection);

        connection.transaction(|| {
            let object = P::get(key, ctx)?;

            ctx.check_if_match(&object)?;

            object.patch(patch_data, ctx)
        })
    }
}

#[derive(Debug)]
pub struct PaginateMessage<P, D>(pub D, pub String, pub RequestData, pub PhantomData<P>)
where
    D: Paginator<Model = P>,
    P: Paginate<D>,
    <D::PaginationColumn as Expression>::SqlType: NotNull + SqlOrd,
    <<D::Model as Model>::From as QuerySource>::FromClause: QueryFragment<Pg>,
    Pg: HasSqlType<<D::PaginationColumn as Expression>::SqlType>,
    D::PaginationColumn: SelectableExpression<<D::Model as Model>::From>,
    D::PaginationColumn: SelectableExpression<<D::Model as Model>::From>,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: AppearsOnTable<<D::Model as Model>::From>,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: NonAggregate,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: QueryFragment<Pg>;

impl<P, D> Message for PaginateMessage<P, D>
where
    D: Paginator<Model = P>,
    P: Paginate<D> + 'static,
    <D::PaginationColumn as Expression>::SqlType: NotNull + SqlOrd,
    <<D::Model as Model>::From as QuerySource>::FromClause: QueryFragment<Pg>,
    Pg: HasSqlType<<D::PaginationColumn as Expression>::SqlType>,
    D::PaginationColumn: SelectableExpression<<D::Model as Model>::From>,
        D::PaginationColumn: SelectableExpression<<D::Model as Model>::From>,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: AppearsOnTable<<D::Model as Model>::From>,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: NonAggregate,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: QueryFragment<Pg>,
{
    type Result = Result<(Vec<P>, String)>;
}

impl<P, D> Handler<PaginateMessage<P, D>> for DatabaseActor
where
    D: Paginator<Model = P>,
    P: Paginate<D> + 'static,
    <D::PaginationColumn as Expression>::SqlType: NotNull + SqlOrd,
    <<D::Model as Model>::From as QuerySource>::FromClause: QueryFragment<Pg>,
    Pg: HasSqlType<<D::PaginationColumn as Expression>::SqlType>,
    D::PaginationColumn: SelectableExpression<<D::Model as Model>::From>,
    D::PaginationColumn: SelectableExpression<<D::Model as Model>::From>,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: AppearsOnTable<<D::Model as Model>::From>,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: NonAggregate,
    <D::PaginationColumnType as AsExpression<
        <D::PaginationColumn as Expression>::SqlType,
    >>::Expression: QueryFragment<Pg>,
{
    type Result = Result<(Vec<P>, String)>;

    fn handle(&mut self, msg: PaginateMessage<P, D>, _: &mut Self::Context) -> Self::Result {
        let connection = &*self.connection_for(&msg.2)?;
        let ctx = msg.2.ctx(connection);

        let result = P::load(&msg.0, ctx)?;

        let first = msg.0.first(connection)?.map(|d| {
            format!(
                "<{}?{}>; rel=first",msg.1,
                serde_urlencoded::ser::to_string(d).unwrap()
            )
        });
        let last = msg.0.last(connection)?.map(|d| {
            format!(
                "<{}?{}>; rel=last",msg.1,
                serde_urlencoded::ser::to_string(d).unwrap()
            )
        });
        let next = msg.0.next(connection)?.map(|d| {
            format!(
                "<{}?{}>; rel=next",msg.1,
                serde_urlencoded::ser::to_string(d).unwrap()
            )
        });
        let prev = msg.0.prev(connection)?.map(|d| {
            format!(
                "<{}?{}>; rel=prev", msg.1,
                serde_urlencoded::ser::to_string(d).unwrap()
            )
        });

        let header = vec![first, next, prev, last]
            .into_iter()
            .filter_map(|x| x)
            .join_with(",")
            .to_string();

        debug!("Pagination header is: {}", header);

        Ok((result, header))
    }
}
