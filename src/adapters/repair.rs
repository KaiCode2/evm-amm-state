//! Lowering of state-affecting [`RepairAction`](super::RepairAction)s into
//! executable [`ReactiveEffect`]s.
//!
//! A1 emits repair *intentions*; this module turns the V3 liquidity-event
//! intention (`RepairAction::V3TickRange`) into a targeted, hash-pinned
//! [`ResyncRequest`] over exactly the storage slots a `Mint`/`Burn` can dirty,
//! while preserving the observability `Hook` the A1 path already emits. When the
//! pool has no resolvable [`V3StorageLayout`] the repair degrades to a
//! conservative whole-storage invalidation rather than being silently dropped.

use alloy_primitives::U256;
use evm_fork_cache::reactive::{
    InvalidationReason, InvalidationRequest, ReactiveContext, ReactiveEffect, ResyncId,
    ResyncPriority, ResyncReason, ResyncRequest, ResyncTarget,
};

use super::storage::{
    V3StorageLayout, layout_for, v3_tick_bitmap_storage_key_with_base,
    v3_tick_info_storage_keys_with_base, v3_word_position,
};
use super::{AdapterEvent, PoolInstanceId, PoolRegistration, PurgeScope};

/// Lower a `RepairAction::V3TickRange` for `pool` into executable effects:
/// a hash-pinned [`ResyncRequest`] over the boundary tick slots, plus the
/// observability hook the caller appends. With no resolvable storage layout the
/// repair falls back to a whole-storage [`InvalidationRequest`].
pub(crate) fn v3_tick_range_effects(
    pool: &PoolRegistration,
    event: &AdapterEvent,
    tick_lower: i32,
    tick_upper: i32,
    ctx: &ReactiveContext,
    instance: Option<&PoolInstanceId>,
) -> Vec<ReactiveEffect> {
    let Some(address) = pool.key.address() else {
        // V3 pools are address-keyed; an address-less key cannot be targeted.
        return Vec::new();
    };

    let Some(layout) = layout_for(pool) else {
        // Without a layout the protocol-specific slots cannot be named safely,
        // so conservatively invalidate all storage for the pool.
        return vec![ReactiveEffect::Invalidate(InvalidationRequest {
            scope: PurgeScope::AllStorage.into(),
            address,
            reason: InvalidationReason::HandlerRequested,
        })];
    };

    let slots = v3_tick_range_slots(&layout, tick_lower, tick_upper);
    let block = super::reactive::resync_block(ctx);
    vec![ReactiveEffect::Resync(ResyncRequest {
        id: ResyncId::new(super::reactive::resync_id(
            instance, event, address, &slots, &block, ctx,
        )),
        reason: ResyncReason::HandlerRequested,
        block,
        targets: vec![ResyncTarget::StorageSlots { address, slots }],
        priority: ResyncPriority::High,
    })]
}

/// Compute the sorted, deduped slot set a V3 liquidity event over
/// `[tick_lower, tick_upper]` must resync: all four `Tick.Info` slots for each
/// boundary tick, the containing `tickBitmap` word(s) (deduped when the boundary
/// ticks share a word), and the global `liquidity` slot.
///
/// All four info words are refreshed (not just `{0, 3}`): a `Mint`/`Burn` that
/// flips a tick's initialized state sets/clears its `feeGrowthOutside{0,1}X128`
/// (words 1/2), which a later tick-crossing quote reads — so the resync must
/// cover them, matching what the cold-start planner warms.
pub(crate) fn v3_tick_range_slots(
    layout: &V3StorageLayout,
    tick_lower: i32,
    tick_upper: i32,
) -> Vec<U256> {
    let mut slots = Vec::new();

    for tick in [tick_lower, tick_upper] {
        let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
        slots.extend_from_slice(&keys);
    }

    let mut words = [
        v3_word_position(tick_lower, layout.tick_spacing),
        v3_word_position(tick_upper, layout.tick_spacing),
    ];
    words.sort_unstable();
    let mut last_word: Option<i16> = None;
    for word in words {
        if last_word != Some(word) {
            slots.push(v3_tick_bitmap_storage_key_with_base(
                word,
                layout.tick_bitmap_base_slot,
            ));
            last_word = Some(word);
        }
    }

    slots.push(layout.liquidity_slot);

    slots.sort_unstable();
    slots.dedup();
    slots
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::storage::V3StorageLayout;

    /// Golden slot-set check for distinct bitmap words. 2 ticks x 4 info words +
    /// 2 bitmap words + liquidity = 11 deduped slots, matching the independent
    /// reconstruction below.
    #[test]
    fn slot_set_distinct_words() {
        let layout = V3StorageLayout::uniswap(60);
        let (tick_lower, tick_upper) = (60, 15_360);
        let got = v3_tick_range_slots(&layout, tick_lower, tick_upper);

        let mut expected = Vec::new();
        for tick in [tick_lower, tick_upper] {
            let keys = v3_tick_info_storage_keys_with_base(tick, layout.ticks_base_slot);
            expected.extend_from_slice(&keys);
        }
        for word in [
            v3_word_position(tick_lower, layout.tick_spacing),
            v3_word_position(tick_upper, layout.tick_spacing),
        ] {
            expected.push(v3_tick_bitmap_storage_key_with_base(
                word,
                layout.tick_bitmap_base_slot,
            ));
        }
        expected.push(layout.liquidity_slot);
        expected.sort_unstable();
        expected.dedup();

        assert_eq!(got, expected);
        assert_eq!(got.len(), 11);
    }

    /// Boundary ticks in the same bitmap word collapse to one bitmap slot: 2
    /// ticks x 4 info words + 1 shared bitmap word + liquidity = 10 deduped slots.
    #[test]
    fn slot_set_shared_word_dedupes() {
        let layout = V3StorageLayout::uniswap(60);
        let (tick_lower, tick_upper) = (60, 180);
        assert_eq!(
            v3_word_position(tick_lower, layout.tick_spacing),
            v3_word_position(tick_upper, layout.tick_spacing),
        );

        let got = v3_tick_range_slots(&layout, tick_lower, tick_upper);
        assert_eq!(got.len(), 10);
    }
}
