/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c) ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use crate::zssp::NoiseXKBobHandshakeState;
use crate::{proto::MAX_UNASSOCIATED_HANDSHAKE_STATES, ApplicationLayer};

pub(crate) struct UnassociatedHandshakeCache<Application: ApplicationLayer> {
    has_pending: AtomicBool, // Allowed to be falsely positive
    cache: RwLock<CacheInner<Application>>,
}
/// SoA format
struct CacheInner<Application: ApplicationLayer> {
    local_ids: [Option<NonZeroU32>; MAX_UNASSOCIATED_HANDSHAKE_STATES],
    timeouts: [i64; MAX_UNASSOCIATED_HANDSHAKE_STATES],
    handshakes: [Option<Arc<NoiseXKBobHandshakeState<Application>>>; MAX_UNASSOCIATED_HANDSHAKE_STATES],
}

/// Linear-search cache for capping the memory consumption of handshake data.
/// Designed specifically to have short and simple code that clearly bounds above
/// memory consumption.
impl<Application: ApplicationLayer> UnassociatedHandshakeCache<Application> {
    pub(crate) fn new() -> Self {
        Self {
            has_pending: AtomicBool::new(false),
            cache: RwLock::new(CacheInner {
                local_ids: std::array::from_fn(|_| None),
                timeouts: std::array::from_fn(|_| 0),
                handshakes: std::array::from_fn(|_| None),
            }),
        }
    }
    pub(crate) fn get(&self, local_id: NonZeroU32) -> Option<Arc<NoiseXKBobHandshakeState<Application>>> {
        let cache = self.cache.read().unwrap();
        for (i, id) in cache.local_ids.iter().enumerate() {
            if *id == Some(local_id) {
                return cache.handshakes[i].clone();
            }
        }
        None
    }
    pub(crate) fn insert(&self, local_id: NonZeroU32, state: Arc<NoiseXKBobHandshakeState<Application>>, current_time: i64) {
        let mut cache = self.cache.write().unwrap();
        let mut idx = 0;
        for i in 0..cache.local_ids.len() {
            if cache.local_ids[i].is_none() || cache.timeouts[i] < current_time {
                idx = i;
                break;
            } else if cache.local_ids[i] == Some(local_id) {
                return;
            }
        }
        cache.local_ids[idx] = Some(local_id);
        cache.timeouts[idx] = current_time.saturating_add(Application::INITIAL_OFFER_TIMEOUT_MS);
        cache.handshakes[idx] = Some(state);
        self.has_pending.store(true, Ordering::Release);
    }
    pub(crate) fn remove(&self, local_id: NonZeroU32) -> bool {
        let mut cache = self.cache.write().unwrap();
        for (i, id) in cache.local_ids.iter().enumerate() {
            if *id == Some(local_id) {
                cache.local_ids[i] = None;
                cache.timeouts[i] = 0;
                cache.handshakes[i] = None;
                return true;
            }
        }
        return false;
    }
    pub(crate) fn service(&self, current_time: i64) {
        // Only check for expiration if we have a pending packet.
        // This check is allowed to have false positives for simplicity's sake.
        if self.has_pending.swap(false, Ordering::Acquire) {
            // Check for packet expiration
            let mut cache = self.cache.write().unwrap();
            let mut has_pending = false;
            for i in 0..cache.local_ids.len() {
                if cache.local_ids[i].is_some() {
                    if cache.timeouts[i] < current_time {
                        cache.local_ids[i] = None;
                        cache.timeouts[i] = 0;
                        cache.handshakes[i] = None;
                    } else {
                        has_pending = true;
                    }
                }
            }
            if has_pending {
                self.has_pending.store(true, Ordering::Release);
            }
        }
    }
}
