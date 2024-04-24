//! A collection of shared traits and error types used in Reth.
//!
//! ## Feature Flags
//!
//! - `test-utils`: Export utilities for testing

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

/// Block Execution traits.
pub mod executor;

/// Possible errors when interacting with the chain.
mod error;
pub use error::{RethError, RethResult};

/// Trie error
pub mod trie;

/// BlockchainTree related traits.
pub mod blockchain_tree;
