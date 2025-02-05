// LNP Node: node running lightning network protocol and generalized lightning
// channels.
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use microservices::{rpc, rpc_connection};

use crate::Error;

#[derive(Clone, Debug, Display, From, Api)]
#[api(encoding = "strict")]
#[display(Debug)]
#[non_exhaustive]
pub enum Reply {
    #[api(type = 0x0000)]
    Success,

    #[api(type = 0x0001)]
    #[from]
    Failure(rpc::Failure),
}

impl rpc_connection::Reply for Reply {}

impl From<Error> for rpc::Failure {
    fn from(err: Error) -> Self {
        rpc::Failure {
            code: 1, // Error from LNPD
            info: err.to_string(),
        }
    }
}

impl From<rpc::Failure> for Error {
    fn from(fail: rpc::Failure) -> Self {
        Error::Other(fail.to_string())
    }
}
