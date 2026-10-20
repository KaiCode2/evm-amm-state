//! Immutable header provenance; no protocol-specific decision or provider I/O.
use super::AmmStatePoint;
use alloy_rpc_types_eth::Header;

/// Full header carried by a validated canonical block publication.
///
/// Construction is private: the canonical batch has verified the advertised
/// header hash. Its chain identity is checked by the runtime before ingestion.
/// This does not independently prove consensus/finality or event completeness.
/// Consumers must verify contiguous parent/number/runtime/version history and
/// honor reorg/full-refresh incidents before extending an absence proof.
#[derive(Clone, Debug)]
pub struct AmmCanonicalHeader {
    point: AmmStatePoint,
    header: Header,
}

impl AmmCanonicalHeader {
    /// Called only after canonical-batch hash validation; no public unchecked
    /// constructor can manufacture evidence for a caller-supplied bitfield.
    #[cfg(feature = "live-runtime")]
    pub(crate) fn from_verified_parts(chain_id: u64, header: Header) -> Self {
        Self {
            point: AmmStatePoint::post_block(chain_id, header.inner.number, header.hash),
            header,
        }
    }

    /// Exact chain, number and advertised hash paired with this full header.
    pub const fn point(&self) -> AmmStatePoint {
        self.point
    }

    /// Immutable full canonical header, including parent hash and logs bloom.
    pub const fn header(&self) -> &Header {
        &self.header
    }
}

#[cfg(all(test, feature = "live-runtime"))]
mod tests;
