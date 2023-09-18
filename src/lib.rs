// Copyright (c) 2023 Espresso Systems (espressosys.com)
// This file is part of the Discord Faucet library.
//
// You should have received a copy of the MIT License
// along with the Discord Faucet library. If not, see <https://mit-license.org/>.

mod faucet;
pub(crate) use crate::faucet::*;

mod web;
pub(crate) use web::*;

mod discord;
pub use discord::*;
