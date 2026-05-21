use serde::{Deserialize, Serialize};

use crate::error::Error;

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    rmp_serde::to_vec(value).map_err(Error::Encode)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, Error> {
    rmp_serde::from_slice(bytes).map_err(Error::Decode)
}
