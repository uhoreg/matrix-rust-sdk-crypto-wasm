// Copyright 2022 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(missing_docs, missing_debug_implementations)]
// triggered by wasm_bindgen code
#![allow(clippy::drop_non_drop)]

pub mod attachment;
pub mod backup;
pub mod device;
pub mod encryption;
pub mod error;
pub mod events;
mod future;
pub mod identifiers;
pub mod identities;
mod js;
pub mod machine;
mod macros;
pub mod olm;
pub mod requests;
pub mod responses;
pub mod store;
pub mod sync_events;
mod tracing;
pub mod types;
pub mod verification;
pub mod vodozemac;

use js_sys::JsString;
use wasm_bindgen::prelude::*;

/// Object containing the versions of the Rust libraries we are using.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug)]
pub struct Versions {
    /// The version of the vodozemac crate.
    #[wasm_bindgen(readonly)]
    pub vodozemac: JsString,
    /// The version of the matrix-sdk-crypto crate.
    #[wasm_bindgen(readonly)]
    pub matrix_sdk_crypto: JsString,
    /// The Git commit hash of the crate's source tree at build time.
    #[wasm_bindgen(readonly)]
    pub git_sha: JsString,
    /// The build-time output of the `git describe` command of the source tree
    /// of crate.
    #[wasm_bindgen(readonly)]
    pub git_description: JsString,
}

/// Get the versions of the Rust libraries we are using.
#[wasm_bindgen(js_name = "getVersions")]
pub fn get_versions() -> Versions {
    Versions {
        vodozemac: matrix_sdk_crypto::vodozemac::VERSION.into(),
        matrix_sdk_crypto: matrix_sdk_crypto::VERSION.into(),
        git_description: env!("VERGEN_GIT_DESCRIBE").to_owned().into(),
        git_sha: env!("VERGEN_GIT_SHA").to_owned().into(),
    }
}

/// Run some stuff when the Wasm module is instantiated.
///
/// Right now, it does the following:
///
/// * Redirect Rust panics to JavaScript console.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}
