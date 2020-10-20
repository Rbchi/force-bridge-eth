//! Generated by capsule
//!
//! `main.rs` is used to define rust lang items and modules.
//! See `entry.rs` for the `main` function. 
//! See `error.rs` for the `Error` type.

#![no_std]
#![no_main]
#![feature(lang_items)]
#![feature(alloc_error_handler)]
#![feature(panic_info_message)]

use ckb_std::{
    default_alloc,
};

ckb_std::entry!(program_entry);
default_alloc!();

use ckb_std::high_level::load_tx_hash;
use ckb_env::CkbChainInterface;
use eth_bridge_typescript_lib::verify;

struct CKBChain {}

impl CkbChainInterface for CKBChain {
    fn load_tx_hash(&self) -> [u8; 32] {
        load_tx_hash().expect("load_tx_hash failed")
    }
}

/// program entry
fn program_entry() -> i8 {
    // Call main function and return error code
    let chain = CKBChain {};
    verify(chain)
}

