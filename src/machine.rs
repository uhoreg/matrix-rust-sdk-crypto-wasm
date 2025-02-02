//! The crypto specific Olm objects.

use std::{collections::BTreeMap, ops::Deref, time::Duration};

use futures_util::{pin_mut, StreamExt};
use js_sys::{Array, Function, JsString, Map, Promise, Set};
use matrix_sdk_common::ruma::{
    self, events::secret::request::SecretName, serde::Raw, DeviceKeyAlgorithm, OwnedTransactionId,
    UInt,
};
use matrix_sdk_crypto::{
    backups::MegolmV1BackupKey,
    olm::BackedUpRoomKey,
    store::{DeviceChanges, IdentityChanges},
    types::RoomKeyBackupInfo,
    CryptoStoreError, EncryptionSyncChanges, GossippedSecret,
};
use serde_json::json;
use serde_wasm_bindgen;
use tracing::warn;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};

use crate::{
    backup::{BackupDecryptionKey, BackupKeys, RoomKeyCounts},
    device, encryption,
    error::MegolmDecryptionError,
    future::{future_to_promise, future_to_promise_with_custom_error},
    identifiers, identities,
    js::downcast,
    olm, requests,
    requests::{outgoing_request_to_js_value, CrossSigningBootstrapRequests, ToDeviceRequest},
    responses::{self, response_from_string},
    store,
    store::RoomKeyInfo,
    sync_events,
    types::{self, RoomKeyImportResult, SignatureVerification},
    verification, vodozemac,
};

/// State machine implementation of the Olm/Megolm encryption protocol
/// used for Matrix end to end encryption.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct OlmMachine {
    inner: matrix_sdk_crypto::OlmMachine,
}

#[wasm_bindgen]
impl OlmMachine {
    /// Constructor will always fail. To create a new `OlmMachine`, please use
    /// the `initialize` method.
    ///
    /// Why this pattern? `initialize` returns a `Promise`. Returning a
    // `Promise` from a constructor is not idiomatic in JavaScript.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<OlmMachine, JsError> {
        Err(JsError::new("To build an `OlmMachine`, please use the `initialize` method"))
    }

    /// Create a new `OlmMachine`.
    ///
    /// The created machine will keep the encryption keys either in a IndexedDB
    /// based store, or in a memory store and once the objects is dropped,
    /// the keys will be lost.
    ///
    /// # Arguments
    ///
    /// * `user_id` - represents the unique ID of the user that owns this
    /// machine.
    ///
    /// * `device_id` - represents the unique ID of the device
    /// that owns this machine.
    ///
    /// * `store_name` - The name that should be used to open the IndexedDB
    ///   based database. If this isn't provided, a memory-only store will be
    ///   used. *Note* the memory-only store will lose your E2EE keys when the
    ///   `OlmMachine` gets dropped.
    ///
    /// * `store_passphrase` - The passphrase that should be used to encrypt the
    ///   IndexedDB based
    pub fn initialize(
        user_id: &identifiers::UserId,
        device_id: &identifiers::DeviceId,
        store_name: Option<String>,
        store_passphrase: Option<String>,
    ) -> Promise {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();

        future_to_promise(async move {
            let store = match (store_name, store_passphrase) {
                (Some(store_name), Some(mut store_passphrase)) => {
                    use zeroize::Zeroize;

                    let store = Some(
                        matrix_sdk_indexeddb::IndexeddbCryptoStore::open_with_passphrase(
                            &store_name,
                            &store_passphrase,
                        )
                        .await?,
                    );

                    store_passphrase.zeroize();

                    store
                }

                (Some(store_name), None) => Some(
                    matrix_sdk_indexeddb::IndexeddbCryptoStore::open_with_name(&store_name).await?,
                ),

                (None, Some(_)) => {
                    return Err(anyhow::Error::msg(
                        "The `store_passphrase` has been set, but it has an effect only if \
                        `store_name` is set, which is not; please provide one",
                    ))
                }

                (None, None) => None,
            };

            Ok(OlmMachine {
                inner: match store {
                    // We need this `#[cfg]` because `IndexeddbCryptoStore`
                    // implements `CryptoStore` only on `target_arch =
                    // "wasm32"`. Without that, we could have a compilation
                    // error when checking the entire workspace. In practice,
                    // it doesn't impact this crate because it's always
                    // compiled for `wasm32`.
                    #[cfg(target_arch = "wasm32")]
                    Some(store) => {
                        matrix_sdk_crypto::OlmMachine::with_store(
                            user_id.as_ref(),
                            device_id.as_ref(),
                            store,
                        )
                        .await?
                    }
                    _ => {
                        matrix_sdk_crypto::OlmMachine::new(user_id.as_ref(), device_id.as_ref())
                            .await
                    }
                },
            })
        })
    }

    /// The unique user ID that owns this `OlmMachine` instance.
    #[wasm_bindgen(getter, js_name = "userId")]
    pub fn user_id(&self) -> identifiers::UserId {
        identifiers::UserId::from(self.inner.user_id().to_owned())
    }

    /// The unique device ID that identifies this `OlmMachine`.
    #[wasm_bindgen(getter, js_name = "deviceId")]
    pub fn device_id(&self) -> identifiers::DeviceId {
        identifiers::DeviceId::from(self.inner.device_id().to_owned())
    }

    /// Get the public parts of our Olm identity keys.
    #[wasm_bindgen(getter, js_name = "identityKeys")]
    pub fn identity_keys(&self) -> vodozemac::IdentityKeys {
        self.inner.identity_keys().into()
    }

    /// Get the display name of our own device.
    #[wasm_bindgen(getter, js_name = "displayName")]
    pub fn display_name(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move { Ok(me.display_name().await?) })
    }

    /// Whether automatic transmission of room key requests is enabled.
    ///
    /// Room key requests allow the device to request room keys that it might
    /// have missed in the original share using `m.room_key_request`
    /// events.
    #[wasm_bindgen(getter, js_name = "roomKeyRequestsEnabled")]
    pub fn are_room_key_requests_enabled(&self) -> bool {
        self.inner.are_room_key_requests_enabled()
    }

    /// Enable or disable automatic transmission of room key requests.
    #[wasm_bindgen(setter, js_name = "roomKeyRequestsEnabled")]
    pub fn set_room_key_requests_enabled(&self, enabled: bool) {
        self.inner.set_room_key_requests_enabled(enabled)
    }

    /// Whether room key forwarding is enabled.
    ///
    /// If room key forwarding is enabled, we will automatically reply to
    /// incoming `m.room_key_request` messages from verified devices by
    /// forwarding the requested key (if we have it).
    #[wasm_bindgen(getter, js_name = "roomKeyForwardingEnabled")]
    pub fn is_room_key_forwarding_enabled(&self) -> bool {
        self.inner.is_room_key_forwarding_enabled()
    }

    /// Enable or disable room key forwarding.
    #[wasm_bindgen(setter, js_name = "roomKeyForwardingEnabled")]
    pub fn set_room_key_forwarding_enabled(&self, enabled: bool) {
        self.inner.set_room_key_forwarding_enabled(enabled)
    }

    /// Get the list of users whose devices we are currently tracking.
    ///
    /// A user can be marked for tracking using the
    /// [`update_tracked_users`](#method.update_tracked_users) method.
    ///
    /// Returns a `Set<UserId>`.
    #[wasm_bindgen(js_name = "trackedUsers")]
    pub fn tracked_users(&self) -> Result<Promise, JsError> {
        let set = Set::new(&JsValue::UNDEFINED);
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            for user in me.tracked_users().await? {
                set.add(&identifiers::UserId::from(user).into());
            }
            Ok(set)
        }))
    }

    /// Update the list of tracked users.
    ///
    /// The OlmMachine maintains a list of users whose devices we are keeping
    /// track of: these are known as "tracked users". These must be users
    /// that we share a room with, so that the server sends us updates for
    /// their device lists.
    ///
    /// # Arguments
    ///
    /// * `users` - An array of user ids that should be added to the list of
    ///   tracked users
    ///
    /// Any users that hadn't been seen before will be flagged for a key query
    /// immediately, and whenever `receive_sync_changes` receives a
    /// "changed" notification for that user in the future.
    ///
    /// Users that were already in the list are unaffected.
    #[wasm_bindgen(js_name = "updateTrackedUsers")]
    pub fn update_tracked_users(&self, users: &Array) -> Result<Promise, JsError> {
        let users = user_ids_to_owned_user_ids(users)?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            me.update_tracked_users(users.iter().map(AsRef::as_ref)).await?;
            Ok(JsValue::UNDEFINED)
        }))
    }

    /// Handle to-device events and one-time key counts from a sync
    /// response.
    ///
    /// This will decrypt and handle to-device events returning the
    /// decrypted versions of them.
    ///
    /// To decrypt an event from the room timeline call
    /// `decrypt_room_event`.
    ///
    /// # Arguments
    ///
    /// * `to_device_events`: the JSON-encoded to-device evens from the `/sync`
    ///   response
    /// * `changed_devices`: the mapping of changed and left devices, from the
    ///   `/sync` response
    /// * `one_time_keys_counts`: The number of one-time keys on the server,
    ///   from the `/sync` response. A `Map` from string (encryption algorithm)
    ///   to number (number of keys).
    /// * `unused_fallback_keys`: Optionally, a `Set` of unused fallback keys on
    ///   the server, from the `/sync` response. If this is set, it is used to
    ///   determine if new fallback keys should be uploaded.
    ///
    /// # Returns
    ///
    /// A list of JSON strings, containing the decrypted to-device events.
    #[wasm_bindgen(js_name = "receiveSyncChanges")]
    pub fn receive_sync_changes(
        &self,
        to_device_events: &str,
        changed_devices: &sync_events::DeviceLists,
        one_time_keys_counts: &Map,
        unused_fallback_keys: Option<Set>,
    ) -> Result<Promise, JsError> {
        let to_device_events = serde_json::from_str(to_device_events)?;
        let changed_devices = changed_devices.inner.clone();
        let one_time_keys_counts: BTreeMap<DeviceKeyAlgorithm, UInt> = one_time_keys_counts
            .entries()
            .into_iter()
            .filter_map(|js_value| {
                let pair = Array::from(&js_value.ok()?);
                let (key, value) = (
                    DeviceKeyAlgorithm::from(pair.at(0).as_string()?),
                    UInt::new(pair.at(1).as_f64()? as u64)?,
                );

                Some((key, value))
            })
            .collect();

        // Convert the unused_fallback_keys JS Set to a `Vec<DeviceKeyAlgorithm>`
        let unused_fallback_keys: Option<Vec<DeviceKeyAlgorithm>> =
            unused_fallback_keys.map(|fallback_keys| {
                fallback_keys
                    .values()
                    .into_iter()
                    .filter_map(|js_value| {
                        Some(DeviceKeyAlgorithm::from(js_value.ok()?.as_string()?))
                    })
                    .collect()
            });

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            // we discard the list of updated room keys in the result; JS applications are
            // expected to use register_room_key_updated_callback to receive updated room
            // keys.
            let (decrypted_to_device_events, _) = me
                .receive_sync_changes(EncryptionSyncChanges {
                    to_device_events,
                    changed_devices: &changed_devices,
                    one_time_keys_counts: &one_time_keys_counts,
                    unused_fallback_keys: unused_fallback_keys.as_deref(),

                    // matrix-sdk-crypto does not (currently) use `next_batch_token`.
                    next_batch_token: None,
                })
                .await?;

            Ok(serde_json::to_string(&decrypted_to_device_events)?)
        }))
    }

    /// Get the outgoing requests that need to be sent out.
    ///
    /// This returns a list of values, each of which can be any of:
    ///   * {@link KeysUploadRequest},
    ///   * {@link KeysQueryRequest},
    ///   * {@link KeysClaimRequest},
    ///   * {@link ToDeviceRequest},
    ///   * {@link SignatureUploadRequest},
    ///   * {@link RoomMessageRequest}, or
    ///   * {@link KeysBackupRequest}.
    ///
    /// Those requests need to be sent out to the server and the
    /// responses need to be passed back to the state machine
    /// using {@link OlmMachine.markRequestAsSent}.
    #[wasm_bindgen(js_name = "outgoingRequests")]
    pub fn outgoing_requests(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(me
                .outgoing_requests()
                .await?
                .into_iter()
                .map(outgoing_request_to_js_value)
                .collect::<Result<Vec<JsValue>, _>>()?
                .into_iter()
                .collect::<Array>())
        })
    }

    /// Mark the request with the given request ID as sent (see
    /// `outgoing_requests`).
    ///
    /// Arguments are:
    ///
    /// * `request_id` represents the unique ID of the request that was sent
    ///   out. This is needed to couple the response with the now sent out
    ///   request.
    /// * `response_type` represents the type of the request that was sent out.
    /// * `response` represents the response that was received from the server
    ///   after the outgoing request was sent out.
    #[wasm_bindgen(js_name = "markRequestAsSent")]
    pub fn mark_request_as_sent(
        &self,
        request_id: &str,
        request_type: requests::RequestType,
        response: &str,
    ) -> Result<Promise, JsError> {
        let transaction_id = OwnedTransactionId::from(request_id);
        let response = response_from_string(response)?;
        let incoming_response = responses::OwnedResponse::try_from((request_type, response))?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(me.mark_request_as_sent(&transaction_id, &incoming_response).await.map(|_| true)?)
        }))
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a room key needs to be shared before this
    /// method can be called using the `share_room_key` method.
    ///
    /// `room_id` is the ID of the room for which the message should
    /// be encrypted. `event_type` is the type of the event. `content`
    /// is the plaintext content of the message that should be
    /// encrypted.
    ///
    /// # Panics
    ///
    /// Panics if a group session for the given room wasn't shared
    /// beforehand.
    #[wasm_bindgen(js_name = "encryptRoomEvent")]
    pub fn encrypt_room_event(
        &self,
        room_id: &identifiers::RoomId,
        event_type: String,
        content: &str,
    ) -> Result<Promise, JsError> {
        let room_id = room_id.inner.clone();
        let content = serde_json::from_str(content)?;
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(serde_json::to_string(
                &me.encrypt_room_event_raw(&room_id, event_type.as_ref(), &content).await?,
            )?)
        }))
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event`, the event that should be decrypted.
    /// * `room_id`, the ID of the room where the event was sent to.
    ///
    /// # Returns
    ///
    /// A `Promise` which resolves to a {@link DecryptedRoomEvent} instance, or
    /// rejects with a {@link MegolmDecryptionError} instance.
    #[wasm_bindgen(js_name = "decryptRoomEvent")]
    pub fn decrypt_room_event(
        &self,
        event: &str,
        room_id: &identifiers::RoomId,
    ) -> Result<Promise, JsError> {
        let event: Raw<_> = serde_json::from_str(event)?;
        let room_id = room_id.inner.clone();
        let me = self.inner.clone();

        Ok(future_to_promise_with_custom_error::<
            _,
            responses::DecryptedRoomEvent,
            MegolmDecryptionError,
        >(async move {
            let room_event = me
                .decrypt_room_event(&event, room_id.as_ref())
                .await
                .map_err(|e| MegolmDecryptionError::from(e))?;
            Ok(responses::DecryptedRoomEvent::from(room_event))
        }))
    }

    /// Get encryption info for a decrypted timeline event.
    ///
    /// This recalculates the `EncryptionInfo` data that is returned by
    /// `decryptRoomEvent`, based on the current
    /// verification status of the sender, etc.
    ///
    /// Returns an error for an unencrypted event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event to get information for.
    /// * `room_id` - The ID of the room where the event was sent to.
    ///
    /// # Returns
    ///
    /// {@link EncryptionInfo}
    #[wasm_bindgen(js_name = "getRoomEventEncryptionInfo")]
    pub fn get_room_event_encryption_info(
        &self,
        event: &str,
        room_id: &identifiers::RoomId,
    ) -> Result<Promise, JsError> {
        let event: Raw<_> = serde_json::from_str(event)?;
        let room_id = room_id.inner.clone();
        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            let encryption_info =
                me.get_room_event_encryption_info(&event, room_id.as_ref()).await?;
            Ok(responses::EncryptionInfo::from(encryption_info))
        }))
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we
    /// have stored locally.
    #[wasm_bindgen(js_name = "crossSigningStatus")]
    pub fn cross_signing_status(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise::<_, olm::CrossSigningStatus>(async move {
            Ok(me.cross_signing_status().await.into())
        })
    }

    /// Export all the private cross signing keys we have.
    ///
    /// The export will contain the seeds for the ed25519 keys as
    /// unpadded base64 encoded strings.
    ///
    /// Returns `null` if we don’t have any private cross signing keys;
    /// otherwise returns a `CrossSigningKeyExport`.
    #[wasm_bindgen(js_name = "exportCrossSigningKeys")]
    pub fn export_cross_signing_keys(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(me.export_cross_signing_keys().await?.map(store::CrossSigningKeyExport::from))
        })
    }

    /// Import our private cross signing keys.
    ///
    /// The keys should be provided as unpadded-base64-encoded strings.
    ///
    /// Returns a `CrossSigningStatus`.
    #[wasm_bindgen(js_name = "importCrossSigningKeys")]
    pub fn import_cross_signing_keys(
        &self,
        master_key: Option<String>,
        self_signing_key: Option<String>,
        user_signing_key: Option<String>,
    ) -> Promise {
        let me = self.inner.clone();
        let export = matrix_sdk_crypto::store::CrossSigningKeyExport {
            master_key,
            self_signing_key,
            user_signing_key,
        };

        future_to_promise(async move {
            Ok(me.import_cross_signing_keys(export).await.map(olm::CrossSigningStatus::from)?)
        })
    }

    /// Create a new cross signing identity and get the upload request
    /// to push the new public keys to the server.
    ///
    /// Warning: This will delete any existing cross signing keys that
    /// might exist on the server and thus will reset the trust
    /// between all the devices.
    ///
    /// Uploading these keys will require user interactive auth.
    ///
    /// # Arguments
    ///
    /// * `reset`, whether the method should create a new identity or use the
    ///   existing one during the request. If set to true, the request will
    ///   attempt to upload a new identity. If set to false, the request will
    ///   attempt to upload the existing identity. Since the uploading process
    ///   requires user interactive authentication, which involves sending out
    ///   the same request multiple times, setting this argument to false
    ///   enables you to reuse the same request.
    ///
    /// Returns a {@link CrossSigningBootstrapRequests}.
    #[wasm_bindgen(js_name = "bootstrapCrossSigning")]
    pub fn bootstrap_cross_signing(&self, reset: bool) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            let requests = me.bootstrap_cross_signing(reset).await?;
            Ok(CrossSigningBootstrapRequests::try_from(requests)?)
        })
    }

    /// Get the cross signing user identity of a user.
    ///
    /// Returns a promise for an `OwnUserIdentity`, a `UserIdentity`, or
    /// `undefined`.
    #[wasm_bindgen(js_name = "getIdentity")]
    pub fn get_identity(&self, user_id: &identifiers::UserId) -> Promise {
        let me = self.inner.clone();
        let user_id = user_id.inner.clone();

        future_to_promise(async move {
            // wait for up to a second for any in-flight device list requests to complete.
            // The reason for this isn't so much to avoid races, but to make testing easier.
            Ok(me
                .get_identity(user_id.as_ref(), Some(Duration::from_secs(1)))
                .await?
                .map(identities::UserIdentities::from))
        })
    }

    /// Sign the given message using our device key and if available
    /// cross-signing master key.
    pub fn sign(&self, message: String) -> Promise {
        let me = self.inner.clone();

        future_to_promise::<_, types::Signatures>(
            async move { Ok(me.sign(&message).await?.into()) },
        )
    }

    /// Invalidate the currently active outbound group session for the
    /// given room.
    ///
    /// Returns true if a session was invalidated, false if there was
    /// no session to invalidate.
    #[wasm_bindgen(js_name = "invalidateGroupSession")]
    pub fn invalidate_group_session(&self, room_id: &identifiers::RoomId) -> Promise {
        let room_id = room_id.inner.clone();
        let me = self.inner.clone();

        future_to_promise(async move { Ok(me.invalidate_group_session(&room_id).await?) })
    }

    /// Get to-device requests to share a room key with users in a room.
    ///
    /// `room_id` is the room ID. `users` is an array of `UserId`
    /// objects. `encryption_settings` are an `EncryptionSettings`
    /// object.
    ///
    /// Returns an array of `ToDeviceRequest`s.
    #[wasm_bindgen(js_name = "shareRoomKey")]
    pub fn share_room_key(
        &self,
        room_id: &identifiers::RoomId,
        users: &Array,
        encryption_settings: &encryption::EncryptionSettings,
    ) -> Result<Promise, JsError> {
        let room_id = room_id.inner.clone();
        let users = user_ids_to_owned_user_ids(users)?;
        let encryption_settings =
            matrix_sdk_crypto::olm::EncryptionSettings::from(encryption_settings);

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            let to_device_requests = me
                .share_room_key(&room_id, users.iter().map(AsRef::as_ref), encryption_settings)
                .await?;

            // convert each request to our own ToDeviceRequest struct, and then wrap it in a
            // JsValue.
            //
            // Then collect the results into a javascript Array, throwing any errors into
            // the promise.
            Ok(to_device_requests
                .into_iter()
                .map(|td| ToDeviceRequest::try_from(td.deref()).map(JsValue::from))
                .collect::<Result<Array, _>>()?)
        }))
    }

    /// Generate an "out-of-band" key query request for the given set of users.
    ///
    /// This can be useful if we need the results from `getIdentity` or
    /// `getUserDevices` to be as up-to-date as possible.
    ///
    /// Returns a `KeysQueryRequest` object. The response of the request should
    /// be passed to the `OlmMachine` with the `mark_request_as_sent`.
    #[wasm_bindgen(js_name = "queryKeysForUsers")]
    pub fn query_keys_for_users(
        &self,
        users: &Array,
    ) -> Result<requests::KeysQueryRequest, JsError> {
        let users = user_ids_to_owned_user_ids(users)?;

        let (request_id, request) =
            self.inner.query_keys_for_users(users.iter().map(AsRef::as_ref));

        Ok(requests::KeysQueryRequest::try_from((request_id.to_string(), &request))?)
    }

    /// Get the a key claiming request for the user/device pairs that
    /// we are missing Olm sessions for.
    ///
    /// Returns `null` if no key claiming request needs to be sent
    /// out, otherwise it returns a `KeysClaimRequest` object.
    ///
    /// Sessions need to be established between devices so group
    /// sessions for a room can be shared with them.
    ///
    /// This should be called every time a group session needs to be
    /// shared as well as between sync calls. After a sync some
    /// devices may request room keys without us having a valid Olm
    /// session with them, making it impossible to server the room key
    /// request, thus it’s necessary to check for missing sessions
    /// between sync as well.
    ///
    /// Note: Care should be taken that only one such request at a
    /// time is in flight, e.g. using a lock.
    ///
    /// The response of a successful key claiming requests needs to be
    /// passed to the `OlmMachine` with the `mark_request_as_sent`.
    ///
    /// `users` represents the list of users that we should check if
    /// we lack a session with one of their devices. This can be an
    /// empty iterator when calling this method between sync requests.
    #[wasm_bindgen(js_name = "getMissingSessions")]
    pub fn get_missing_sessions(&self, users: &Array) -> Result<Promise, JsError> {
        let users = user_ids_to_owned_user_ids(users)?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            match me.get_missing_sessions(users.iter().map(AsRef::as_ref)).await? {
                Some((transaction_id, keys_claim_request)) => {
                    Ok(JsValue::from(requests::KeysClaimRequest::try_from((
                        transaction_id.to_string(),
                        &keys_claim_request,
                    ))?))
                }

                None => Ok(JsValue::NULL),
            }
        }))
    }

    /// Get a map holding all the devices of a user.
    ///
    /// ### Parameters
    ///
    /// * `user_id` - The unique ID of the user that the device belongs to.
    ///
    /// * `timeout_secs` - The amount of time we should wait for a `/keys/query`
    ///   response before returning if the user's device list has been marked as
    ///   stale. **Note**, this assumes that the requests from {@link
    ///   OlmMachine.outgoingRequests} are being processed and sent out.
    ///
    ///   If unset, we will return immediately even if the device list is stale.
    ///
    /// ### Returns
    ///
    /// A {@link UserDevices} object.
    #[wasm_bindgen(js_name = "getUserDevices")]
    pub fn get_user_devices(
        &self,
        user_id: &identifiers::UserId,
        timeout_secs: Option<f64>,
    ) -> Promise {
        let user_id = user_id.inner.clone();
        let timeout_duration = timeout_secs.map(Duration::from_secs_f64);

        let me = self.inner.clone();

        future_to_promise::<_, device::UserDevices>(async move {
            Ok(me.get_user_devices(&user_id, timeout_duration).await.map(Into::into)?)
        })
    }

    /// Get a specific device of a user.
    ///
    /// ### Parameters
    ///
    /// * `user_id` - The unique ID of the user that the device belongs to.
    ///
    /// * `device_id` - The unique ID of the device.
    ///
    /// * `timeout_secs` - The amount of time we should wait for a `/keys/query`
    ///   response before returning if the user's device list has been marked as
    ///   stale. **Note**, this assumes that the requests from {@link
    ///   OlmMachine.outgoingRequests} are being processed and sent out.
    ///
    ///   If unset, we will return immediately even if the device list is stale.
    ///
    /// ### Returns
    ///
    /// If the device is known, a {@link Device}. Otherwise, `undefined`.
    #[wasm_bindgen(js_name = "getDevice")]
    pub fn get_device(
        &self,
        user_id: &identifiers::UserId,
        device_id: &identifiers::DeviceId,
        timeout_secs: Option<f64>,
    ) -> Promise {
        let user_id = user_id.inner.clone();
        let device_id = device_id.inner.clone();
        let timeout_duration = timeout_secs.map(Duration::from_secs_f64);

        let me = self.inner.clone();

        future_to_promise::<_, Option<device::Device>>(async move {
            Ok(me.get_device(&user_id, &device_id, timeout_duration).await?.map(Into::into))
        })
    }

    /// Get a verification object for the given user ID with the given
    /// flow ID (a to-device request ID if the verification has been
    /// requested by a to-device request, or a room event ID if the
    /// verification has been requested by a room event).
    ///
    /// It returns a “`Verification` object”, which is either a `Sas`
    /// or `Qr` object.
    #[wasm_bindgen(js_name = "getVerification")]
    pub fn get_verification(
        &self,
        user_id: &identifiers::UserId,
        flow_id: &str,
    ) -> Result<JsValue, JsError> {
        self.inner
            .get_verification(&user_id.inner, flow_id)
            .map(verification::Verification)
            .map(JsValue::try_from)
            .transpose()
            .map(JsValue::from)
    }

    /// Get a verification request object with the given flow ID.
    #[wasm_bindgen(js_name = "getVerificationRequest")]
    pub fn get_verification_request(
        &self,
        user_id: &identifiers::UserId,
        flow_id: &str,
    ) -> Option<verification::VerificationRequest> {
        self.inner.get_verification_request(&user_id.inner, flow_id).map(Into::into)
    }

    /// Get all the verification requests of a given user.
    #[wasm_bindgen(js_name = "getVerificationRequests")]
    pub fn get_verification_requests(&self, user_id: &identifiers::UserId) -> Array {
        self.inner
            .get_verification_requests(&user_id.inner)
            .into_iter()
            .map(verification::VerificationRequest::from)
            .map(JsValue::from)
            .collect()
    }

    /// Receive a verification event.
    ///
    /// This method can be used to pass verification events that are happening
    /// in rooms to the `OlmMachine`. The event should be in the decrypted form.
    #[wasm_bindgen(js_name = "receiveVerificationEvent")]
    pub fn receive_verification_event(
        &self,
        event: &str,
        room_id: &identifiers::RoomId,
    ) -> Result<Promise, JsError> {
        let room_id = room_id.inner.clone();
        let event: ruma::events::AnySyncMessageLikeEvent = serde_json::from_str(event)?;
        let event = event.into_full_event(room_id);

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            Ok(me.receive_verification_event(&event).await.map(|_| JsValue::UNDEFINED)?)
        }))
    }

    /// Export the keys that match the given predicate.
    ///
    /// `predicate` is a closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true`, the `InboundGroupSession` will be included in the
    /// export, otherwise it won't.
    #[wasm_bindgen(js_name = "exportRoomKeys")]
    pub fn export_room_keys(&self, predicate: Function) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            Ok(serde_json::to_string(
                &me.export_room_keys(|session| {
                    let session = session.clone();

                    predicate
                        .call1(&JsValue::NULL, &olm::InboundGroupSession::from(session).into())
                        .expect("Predicate function passed to `export_room_keys` failed")
                        .as_bool()
                        .unwrap_or(false)
                })
                .await?,
            )?)
        })
    }

    /// Import the given room keys into our store.
    ///
    /// Mostly, a deprecated alias for `importExportedRoomKeys`, though the
    /// return type is different.
    ///
    /// Returns a String containing a JSON-encoded object, holding three
    /// properties:
    ///  * `total_count` (the total number of keys found in the export data).
    ///  * `imported_count` (the number of keys that were imported).
    ///  * `keys` (the keys that were imported; a map from room id to a map of
    ///    the sender key to a list of session ids).
    ///
    /// @deprecated Use `importExportedRoomKeys` or `importBackedUpRoomKeys`.
    #[wasm_bindgen(js_name = "importRoomKeys")]
    pub fn import_room_keys(
        &self,
        exported_room_keys: &str,
        progress_listener: Function,
    ) -> Result<Promise, JsError> {
        let me = self.inner.clone();
        let exported_room_keys = serde_json::from_str(exported_room_keys)?;

        Ok(future_to_promise(async move {
            let matrix_sdk_crypto::RoomKeyImportResult { imported_count, total_count, keys } =
                Self::import_exported_room_keys_helper(&me, exported_room_keys, progress_listener)
                    .await?;

            Ok(serde_json::to_string(&json!({
                "imported_count": imported_count,
                "total_count": total_count,
                "keys": keys,
            }))?)
        }))
    }

    /// Import the given room keys into our store.
    ///
    /// `exported_keys` is a JSON-encoded list of previously exported keys that
    /// should be imported into our store. If we already have a better
    /// version of a key, the key will _not_ be imported.
    ///
    /// `progress_listener` is a closure that takes 2 `BigInt` arguments:
    /// `progress` and `total`, and returns nothing.
    ///
    /// Returns a {@link RoomKeyImportResult}.
    #[wasm_bindgen(js_name = "importExportedRoomKeys")]
    pub fn import_exported_room_keys(
        &self,
        exported_room_keys: &str,
        progress_listener: Function,
    ) -> Result<Promise, JsError> {
        let me = self.inner.clone();
        let exported_room_keys = serde_json::from_str(exported_room_keys)?;

        Ok(future_to_promise(async move {
            let result: RoomKeyImportResult =
                Self::import_exported_room_keys_helper(&me, exported_room_keys, progress_listener)
                    .await?
                    .into();
            Ok(result)
        }))
    }

    /// Shared helper for `import_exported_room_keys` and `import_room_keys`.
    ///
    /// Wraps the progress listener in a Rust closure and runs
    /// `Store::import_exported_room_keys`
    async fn import_exported_room_keys_helper(
        inner: &matrix_sdk_crypto::OlmMachine,
        exported_room_keys: Vec<matrix_sdk_crypto::olm::ExportedRoomKey>,
        progress_listener: Function,
    ) -> Result<matrix_sdk_crypto::RoomKeyImportResult, CryptoStoreError> {
        inner
            .store()
            .import_exported_room_keys(exported_room_keys, |progress, total| {
                progress_listener
                    .call2(&JsValue::NULL, &JsValue::from(progress), &JsValue::from(total))
                    .expect("Progress listener passed to `importExportedRoomKeys` failed");
            })
            .await
    }

    /// Import the given room keys into our store.
    ///
    /// # Arguments
    ///
    /// * `backed_up_room_keys`: keys that were retrieved from backup and that
    ///   should be added to our store (provided they are better than our
    ///   current versions of those keys). Specifically, it should be a Map from
    ///   {@link RoomId}, to a Map from session ID to a (decrypted) session data
    ///   structure.
    ///
    /// * `progress_listener`: an optional callback that takes 2 arguments:
    ///   `progress` and `total`, and returns nothing.
    ///
    /// # Returns
    ///
    /// A {@link RoomKeyImportResult}.
    #[wasm_bindgen(js_name = "importBackedUpRoomKeys")]
    pub fn import_backed_up_room_keys(
        &self,
        backed_up_room_keys: &Map,
        progress_listener: Option<Function>,
    ) -> Result<Promise, JsValue> {
        let me = self.inner.clone();

        // convert the js-side data into rust data
        let mut keys: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        for backed_up_room_keys_entry in backed_up_room_keys.entries() {
            let backed_up_room_keys_entry: Array = backed_up_room_keys_entry?.dyn_into()?;
            let room_id =
                &downcast::<identifiers::RoomId>(&backed_up_room_keys_entry.get(0), "RoomId")?
                    .inner;

            let room_room_keys: Map = backed_up_room_keys_entry.get(1).dyn_into()?;

            for room_room_keys_entry in room_room_keys.entries() {
                let room_room_keys_entry: Array = room_room_keys_entry?.dyn_into()?;
                let session_id: JsString = room_room_keys_entry.get(0).dyn_into()?;
                let key: BackedUpRoomKey =
                    serde_wasm_bindgen::from_value(room_room_keys_entry.get(1))?;

                keys.entry(room_id.clone()).or_default().insert(session_id.into(), key);
            }
        }

        Ok(future_to_promise(async move {
            let result: RoomKeyImportResult = me
                .backup_machine()
                .import_backed_up_room_keys(keys, |progress, total| {
                    if let Some(callback) = &progress_listener {
                        callback
                            .call2(&JsValue::NULL, &JsValue::from(progress), &JsValue::from(total))
                            .expect("Progress listener passed to `importBackedUpRoomKeys` failed");
                    }
                })
                .await?
                .into();
            Ok(result)
        }))
    }

    /// Store the backup decryption key in the crypto store.
    ///
    /// This is useful if the client wants to support gossiping of the backup
    /// key.
    ///
    /// Returns `Promise<void>`.
    #[wasm_bindgen(js_name = "saveBackupDecryptionKey")]
    pub fn save_backup_decryption_key(
        &self,
        decryption_key: &BackupDecryptionKey,
        version: String,
    ) -> Promise {
        let me = self.inner.clone();
        let inner_key = decryption_key.inner.clone();

        future_to_promise(async move {
            me.backup_machine().save_decryption_key(Some(inner_key), Some(version)).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Get the backup keys we have saved in our store.
    /// Returns a `Promise` for {@link BackupKeys}.
    #[wasm_bindgen(js_name = "getBackupKeys")]
    pub fn get_backup_keys(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            let inner = me.backup_machine().get_backup_keys().await?;
            Ok(BackupKeys {
                decryption_key: inner.decryption_key.map(|k| k.clone().into()),
                backup_version: inner.backup_version,
            })
        })
    }

    /// Check if the given backup has been verified by us or by another of our
    /// devices that we trust.
    ///
    /// The `backup_info` should be a Javascript object with the following
    /// format:
    ///
    /// ```json
    /// {
    ///     "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
    ///     "auth_data": {
    ///         "public_key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
    ///         "signatures": {}
    ///     }
    /// }
    /// ```
    ///
    /// Returns a {@link SignatureVerification} object.
    #[wasm_bindgen(js_name = "verifyBackup")]
    pub fn verify_backup(&self, backup_info: JsValue) -> Result<Promise, JsError> {
        let backup_info: RoomKeyBackupInfo = serde_wasm_bindgen::from_value(backup_info)?;

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            let result = me.backup_machine().verify_backup(backup_info, false).await?;
            Ok(SignatureVerification { inner: result })
        }))
    }

    /// Activate the given backup key to be used with the given backup version.
    ///
    /// **Warning**: The caller needs to make sure that the given `BackupKey` is
    /// trusted, otherwise we might be encrypting room keys that a malicious
    /// party could decrypt.
    ///
    /// The {@link #verifyBackup} method can be used to do so.
    ///
    /// Returns `Promise<void>`.
    #[wasm_bindgen(js_name = "enableBackupV1")]
    pub fn enable_backup_v1(
        &self,
        public_key_base_64: String,
        version: String,
    ) -> Result<Promise, JsError> {
        let backup_key = MegolmV1BackupKey::from_base64(&public_key_base_64)?;
        backup_key.set_version(version);

        let me = self.inner.clone();

        Ok(future_to_promise(async move {
            me.backup_machine().enable_backup_v1(backup_key).await?;
            Ok(JsValue::UNDEFINED)
        }))
    }

    /// Are we able to encrypt room keys.
    ///
    /// This returns true if we have an active `BackupKey` and backup version
    /// registered with the state machine.
    ///
    /// Returns `Promise<bool>`.
    #[wasm_bindgen(js_name = "isBackupEnabled")]
    pub fn is_backup_enabled(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            let enabled = me.backup_machine().enabled().await;
            Ok(JsValue::from_bool(enabled))
        })
    }

    /// Disable and reset our backup state.
    ///
    /// This will remove any pending backup request, remove the backup key and
    /// reset the backup state of each room key we have.
    ///
    /// Returns `Promise<void>`.
    #[wasm_bindgen(js_name = "disableBackup")]
    pub fn disable_backup(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            me.backup_machine().disable_backup().await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Encrypt a batch of room keys and return a request that needs to be sent
    /// out to backup the room keys.
    ///
    /// Returns an optional {@link KeysBackupRequest}.
    #[wasm_bindgen(js_name = "backupRoomKeys")]
    pub fn backup_room_keys(&self) -> Promise {
        let me = self.inner.clone();

        future_to_promise(async move {
            match me.backup_machine().backup().await? {
                Some((transaction_id, keys_backup_request)) => {
                    Ok(Some(requests::KeysBackupRequest::try_from((
                        transaction_id.to_string(),
                        &keys_backup_request,
                    ))?))
                }

                None => Ok(None),
            }
        })
    }

    /// Get the number of backed up room keys and the total number of room keys.
    /// Returns a {@link RoomKeyCounts}.
    #[wasm_bindgen(js_name = "roomKeyCounts")]
    pub fn room_key_counts(&self) -> Promise {
        let me = self.inner.clone();
        future_to_promise::<_, RoomKeyCounts>(async move {
            Ok(me.backup_machine().room_key_counts().await?.into())
        })
    }

    /// Encrypt the list of exported room keys using the given passphrase.
    ///
    /// `exported_room_keys` is a list of sessions that should be encrypted
    /// (it's generally returned by `export_room_keys`). `passphrase` is the
    /// passphrase that will be used to encrypt the exported room keys. And
    /// `rounds` is the number of rounds that should be used for the key
    /// derivation when the passphrase gets turned into an AES key. More rounds
    /// are increasingly computationnally intensive and as such help against
    /// brute-force attacks. Should be at least `10_000`, while values in the
    /// `100_000` ranges should be preferred.
    #[wasm_bindgen(js_name = "encryptExportedRoomKeys")]
    pub fn encrypt_exported_room_keys(
        exported_room_keys: &str,
        passphrase: &str,
        rounds: u32,
    ) -> Result<String, JsError> {
        let exported_room_keys: Vec<matrix_sdk_crypto::olm::ExportedRoomKey> =
            serde_json::from_str(exported_room_keys)?;

        Ok(matrix_sdk_crypto::encrypt_room_key_export(&exported_room_keys, passphrase, rounds)?)
    }

    /// Try to decrypt a reader into a list of exported room keys.
    ///
    /// `encrypted_exported_room_keys` is the result from
    /// `encrypt_exported_room_keys`. `passphrase` is the passphrase that was
    /// used when calling `encrypt_exported_room_keys`.
    #[wasm_bindgen(js_name = "decryptExportedRoomKeys")]
    pub fn decrypt_exported_room_keys(
        encrypted_exported_room_keys: &str,
        passphrase: &str,
    ) -> Result<String, JsError> {
        Ok(serde_json::to_string(&matrix_sdk_crypto::decrypt_room_key_export(
            encrypted_exported_room_keys.as_bytes(),
            passphrase,
        )?)?)
    }

    /// Register a callback which will be called whenever there is an update to
    /// a room key.
    ///
    /// `callback` should be a function that takes a single argument (an array
    /// of {@link RoomKeyInfo}) and returns a Promise.
    #[wasm_bindgen(js_name = "registerRoomKeyUpdatedCallback")]
    pub async fn register_room_key_updated_callback(&self, callback: Function) {
        let stream = self.inner.store().room_keys_received_stream();

        // fire up a promise chain which will call `cb` on each result from the stream
        spawn_local(async move {
            // take a reference to `callback` (which we then pass into the closure), to stop
            // the callback being moved into the closure (which would mean we could only
            // call the closure once)
            let callback_ref = &callback;
            stream.for_each(move |item| send_room_key_info_to_callback(callback_ref, item)).await;
        });
    }

    /// Register a callback which will be called whenever there is an update to
    /// a user identity.
    ///
    /// `callback` should be a function that takes a single argument (a {@link
    /// UserId}) and returns a Promise.
    #[wasm_bindgen(js_name = "registerUserIdentityUpdatedCallback")]
    pub async fn register_user_identity_updated_callback(&self, callback: Function) {
        let stream = self.inner.store().identities_stream_raw();

        // fire up a promise chain which will call `cb` on each result from the stream
        spawn_local(async move {
            // take a reference to `callback` (which we then pass into the closure), to stop
            // the callback being moved into the closure (which would mean we could only
            // call the closure once)
            let callback_ref = &callback;
            stream.for_each(move |item| send_user_identities_to_callback(callback_ref, item)).await;
        });
    }

    /// Register a callback which will be called whenever a secret
    /// (`m.secret.send`) is received.
    ///
    /// The only secret this will currently broadcast is the
    /// `m.megolm_backup.v1` (the cross signing secrets are handled internally).
    ///
    /// To request a secret from other devices, a client sends an
    /// `m.secret.request` device event with `action` set to `request` and
    /// `name` set to the identifier of the secret. A device that wishes to
    /// share the secret will reply with an `m.secret.send` event, encrypted
    /// using olm.
    ///
    /// The secrets are guaranteed to have been received over a 1-to-1 encrypted
    /// to_device message from a one of the user's own verified devices.
    ///
    /// See https://matrix-org.github.io/matrix-rust-sdk/matrix_sdk_crypto/store/struct.Store.html#method.secrets_stream for more information.
    ///
    /// `callback` should be a function that takes 2 arguments: the secret name
    /// (string) and value (string).
    ///
    /// **Note**: if the secret is valid and handled on the javascript side, the
    /// secret inbox should be cleared by calling
    /// `delete_secrets_from_inbox`.
    #[wasm_bindgen(js_name = "registerReceiveSecretCallback")]
    pub async fn register_receive_secret_callback(&self, callback: Function) {
        let stream = self.inner.store().secrets_stream();
        // fire up a promise chain which will call `callback` on each result from the
        // stream
        spawn_local(async move {
            // Pin the stream to ensure it can be safely moved across threads
            pin_mut!(stream);
            while let Some(secret) = stream.next().await {
                send_secret_gossip_to_callback(&callback, &secret).await;
            }
        });
    }

    /// Get all the secrets with the given secret_name we have currently
    /// stored.
    /// The only secret this will currently return is the
    /// `m.megolm_backup.v1` secret.
    ///
    /// Usually you would just register a callback with
    /// [`register_receive_secret_callback`], but if the client is shut down
    /// before handling them, this method can be used to retrieve them.
    /// This method should therefore be called at client startup to retrieve any
    /// secrets received during the previous session.
    ///
    /// The secrets are guaranteed to have been received over a 1-to-1 encrypted
    /// to_device message from one of the user's own verified devices.
    ///
    /// Returns a `Promise` for a `Set` of `String` corresponding to the secret
    /// values.
    ///
    /// If the secret is valid and handled, the secret inbox should be cleared
    /// by calling `delete_secrets_from_inbox`.
    #[wasm_bindgen(js_name = "getSecretsFromInbox")]
    pub async fn get_secrets_from_inbox(&self, secret_name: String) -> Promise {
        let set = Set::new(&JsValue::UNDEFINED);
        let me = self.inner.clone();

        future_to_promise(async move {
            let name = SecretName::from(secret_name);
            for gossip in me.store().get_secrets_from_inbox(&name).await? {
                set.add(&JsValue::from_str(&gossip.event.content.secret));
            }
            Ok(set)
        })
    }

    /// Delete all secrets with the given secret name from the inbox.
    ///
    /// Should be called after handling the secrets with
    /// `get_secrets_from_inbox`.
    ///
    /// # Arguments
    ///
    /// * `secret_name` - The name of the secret to delete.
    #[wasm_bindgen(js_name = "deleteSecretsFromInbox")]
    pub async fn delete_secrets_from_inbox(&self, secret_name: String) -> Promise {
        let me = self.inner.clone();
        future_to_promise(async move {
            let name = SecretName::from(secret_name);
            me.store().delete_secrets_from_inbox(&name).await?;
            Ok(JsValue::UNDEFINED)
        })
    }

    /// Request missing local secrets from our other trusted devices.
    ///
    /// "Local secrets" refers to secrets which can be shared between trusted
    /// devices, such as private cross-signing keys, and the megolm backup
    /// decryption key.
    ///
    /// This method will cause the sdk to generated outgoing secret requests
    /// (`m.secret.request`) to get the missing secrets. These requests will
    /// then be returned by a future call to {@link
    /// OlmMachine#outgoing_requests}.
    ///
    /// # Returns
    ///
    /// A `Promise` for a `bool` result, which will be true if  secrets were
    /// missing, and a request was generated.
    #[wasm_bindgen(js_name = "requestMissingSecretsIfNeeded")]
    pub async fn request_missing_secrets_if_needed(&self) -> Promise {
        let me = self.inner.clone();
        future_to_promise(async move {
            let has_missing_secrets = me.query_missing_secrets_from_other_sessions().await?;
            Ok(JsValue::from_bool(has_missing_secrets))
        })
    }

    /// Shut down the `OlmMachine`.
    ///
    /// The `OlmMachine` cannot be used after this method has been called.
    ///
    /// All associated resources will be closed too, like IndexedDB
    /// connections.
    pub fn close(self) {}
}

// helper for register_room_key_received_callback: wraps the key info
// into our own RoomKeyInfo struct, and passes it into the javascript
// function
async fn send_room_key_info_to_callback(
    callback: &Function,
    room_key_info: Vec<matrix_sdk_crypto::store::RoomKeyInfo>,
) {
    let rki: Array = room_key_info.into_iter().map(RoomKeyInfo::from).map(JsValue::from).collect();
    match promise_result_to_future(callback.call1(&JsValue::NULL, &rki)).await {
        Ok(_) => (),
        Err(e) => {
            warn!("Error calling room-key-received callback: {:?}", e);
        }
    }
}

// helper for register_user_identity_updated_callback: passes the user ID into
// the javascript function
async fn send_user_identities_to_callback(
    callback: &Function,
    (identity_updates, _): (IdentityChanges, DeviceChanges),
) {
    let update_chain = identity_updates.new.into_iter().chain(identity_updates.changed.into_iter());
    for update in update_chain {
        let user_id = identifiers::UserId::from(update.user_id().to_owned());
        match promise_result_to_future(callback.call1(&JsValue::NULL, &(user_id.into()))).await {
            Ok(_) => (),
            Err(e) => {
                warn!("Error calling user-identity-updated callback: {:?}", e);
            }
        }
    }
}

// helper for register_secret_receive_callback: passes the secret name and value
// into the javascript function
async fn send_secret_gossip_to_callback(callback: &Function, secret: &GossippedSecret) {
    match promise_result_to_future(callback.call2(
        &JsValue::NULL,
        &(secret.secret_name.as_str().into()),
        &(&secret.event.content.secret.to_owned().into()),
    ))
    .await
    {
        Ok(_) => (),
        Err(e) => {
            warn!("Error calling receive secret callback: {:?}", e);
        }
    }
}

/// Given a result from a javascript function which returns a Promise (or throws
/// an exception before returning one), convert the result to a rust Future
/// which completes with the result of the promise
pub(crate) async fn promise_result_to_future(
    res: Result<JsValue, JsValue>,
) -> Result<JsValue, JsValue> {
    match res {
        Ok(retval) => {
            if !retval.has_type::<Promise>() {
                panic!("not a promise");
            }
            let prom: Promise = retval.dyn_into().map_err(|v| {
                JsError::new(&format!("function returned a non-Promise value {v:?}"))
            })?;
            JsFuture::from(prom).await
        }
        Err(e) => {
            // the function threw an exception before it returned the promise. We can just
            // return the error as an error result.
            Err(e)
        }
    }
}

/// Helper function to take a Javascript array of `UserId`s and turn it into
/// a Rust `Vec` of `OwnedUserId`s
fn user_ids_to_owned_user_ids(users: &Array) -> Result<Vec<ruma::OwnedUserId>, JsError> {
    users
        .iter()
        .map(|user| Ok(downcast::<identifiers::UserId>(&user, "UserId")?.inner.clone()))
        .collect()
}
