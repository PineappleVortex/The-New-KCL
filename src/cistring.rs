use derive_more::Display;
use serde::{Deserialize, Serialize, Serializer};
use sqlx::{
    decode::{Decode, DecodeError},
    encode::Encode,
    postgres::PgTypeInfo,
    types::HasSqlType,
    Postgres,
};
use std::{borrow::Borrow, cmp::Ordering, ops::Deref};

// FIXME: probably replace with https://docs.rs/unicase/2.6.0/unicase/struct.UniCase.html after we confirm how exactly postgres CITEXT type works internally

#[derive(Clone, Debug, Hash, Serialize, Deserialize, Display)]
#[serde(transparent)]
pub struct CiString(pub String);

impl From<String> for CiString {
    fn from(string: String) -> Self {
        CiString(string)
    }
}

impl HasSqlType<CiString> for Postgres {
    fn type_info() -> Self::TypeInfo {
        PgTypeInfo::with_oid(16679)
    }
}

impl Encode<Postgres> for CiString {
    fn encode(&self, buf: &mut Vec<u8>) {
        self.0.encode(buf)
    }
}

impl Decode<Postgres> for CiString {
    fn decode(raw: &[u8]) -> Result<Self, DecodeError> {
        String::decode(raw).map(CiString)
    }
}

#[derive(Debug, Hash, Display)]
#[repr(transparent)]
pub struct CiStr(str);

impl Serialize for CiStr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_ref())
    }
}

impl PartialEq<CiString> for CiString {
    fn eq(&self, other: &CiString) -> bool {
        self.0.to_lowercase().eq(&other.0.to_lowercase())
    }
}

impl Eq for CiString {}

impl PartialOrd<CiString> for CiString {
    fn partial_cmp(&self, other: &CiString) -> Option<Ordering> {
        self.0.to_lowercase().partial_cmp(&other.0.to_lowercase())
    }
}

impl PartialEq<CiString> for CiStr {
    fn eq(&self, other: &CiString) -> bool {
        other.0.to_lowercase() == self.0.to_lowercase()
    }
}

impl Ord for CiString {
    fn cmp(&self, other: &CiString) -> Ordering {
        self.0.to_lowercase().cmp(&other.0.to_lowercase())
    }
}

impl AsRef<str> for CiString {
    fn as_ref(&self) -> &str {
        &*self.0
    }
}

impl Borrow<str> for CiString {
    fn borrow(&self) -> &str {
        &*self.0
    }
}

impl Deref for CiString {
    type Target = String;

    fn deref(&self) -> &String {
        &self.0
    }
}

impl AsRef<str> for CiStr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl CiStr {
    pub fn from_str(s: &str) -> &CiStr {
        unsafe { &*(s as *const str as *const CiStr) }
    }
}

impl AsRef<CiStr> for CiString {
    fn as_ref(&self) -> &CiStr {
        CiStr::from_str(self.0.as_ref())
    }
}

impl Deref for CiStr {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl Borrow<CiStr> for CiString {
    fn borrow(&self) -> &CiStr {
        self.as_ref()
    }
}

impl ToOwned for CiStr {
    type Owned = CiString;

    fn to_owned(&self) -> Self::Owned {
        CiString(self.to_string())
    }
}

impl PartialEq for CiStr {
    fn eq(&self, other: &CiStr) -> bool {
        self.to_lowercase().eq(&other.to_lowercase())
    }
}

impl Eq for CiStr {}

impl PartialOrd for CiStr {
    fn partial_cmp(&self, other: &CiStr) -> Option<Ordering> {
        self.to_lowercase().partial_cmp(&other.to_lowercase())
    }
}

impl Ord for CiStr {
    fn cmp(&self, other: &CiStr) -> Ordering {
        self.to_lowercase().cmp(&other.to_lowercase())
    }
}

impl Into<String> for CiString {
    fn into(self) -> String {
        self.0
    }
}
