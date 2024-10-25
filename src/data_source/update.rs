// Copyright (c) 2022 Espresso Systems (espressosys.com)
// This file is part of the HotShot Query Service library.
//
// This program is free software: you can redistribute it and/or modify it under the terms of the GNU
// General Public License as published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
// This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
// even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
// General Public License for more details.
// You should have received a copy of the GNU General Public License along with this program. If not,
// see <https://www.gnu.org/licenses/>.

//! A generic algorithm for updating a HotShot Query Service data source with new data.
use crate::{
    availability::{
        BlockQueryData, LeafQueryData, QueryablePayload, UpdateAvailabilityData, VidCommonQueryData,
    },
    Leaf, Payload,
};
use anyhow::Context;
use async_trait::async_trait;
use futures::future::Future;
use hotshot::types::{Event, EventType};
use hotshot_types::event::LeafInfo;
use hotshot_types::{
    traits::{
        block_contents::{BlockHeader, BlockPayload, EncodeBytes, GENESIS_VID_NUM_STORAGE_NODES},
        node_implementation::{ConsensusTime, NodeType},
    },
    vid::vid_scheme,
};
use jf_vid::VidScheme;
use std::iter::once;

/// An extension trait for types which implement the update trait for each API module.
///
/// If a type implements [UpdateAvailabilityData] and
/// [UpdateStatusData](crate::status::UpdateStatusData), then it can be fully kept up to date
/// through two interfaces:
/// * [populate_metrics](crate::status::UpdateStatusData::populate_metrics), to get a handle for
///   populating the status metrics, which should be used when initializing a
///   [SystemContextHandle](hotshot::types::SystemContextHandle)
/// * [update](Self::update), provided by this extension trait, to update the query state when a new
///   HotShot event is emitted
#[async_trait]
pub trait UpdateDataSource<Types: NodeType>: UpdateAvailabilityData<Types> {
    /// Update query state based on a new consensus event.
    ///
    /// The caller is responsible for authenticating `event`. This function does not perform any
    /// authentication, and if given an invalid `event` (one which does not follow from the latest
    /// known state of the ledger) it may panic or silently accept the invalid `event`. This allows
    /// the best possible performance in the case where the query service and the HotShot instance
    /// are running in the same process (and thus the event stream, directly from HotShot) is
    /// trusted.
    ///
    /// If you want to update the data source with an untrusted event, for example one received from
    /// a peer over the network, you must authenticate it first.
    async fn update(&mut self, event: &Event<Types>) -> anyhow::Result<()>;
}

#[async_trait]
impl<Types: NodeType, T> UpdateDataSource<Types> for T
where
    T: UpdateAvailabilityData<Types> + Send,
    Payload<Types>: QueryablePayload<Types>,
    <Types as NodeType>::InstanceState: Default,
{
    async fn update(&mut self, event: &Event<Types>) -> anyhow::Result<()> {
        if let EventType::Decide { leaf_chain, qc, .. } = &event.event {
            // `qc` justifies the first (most recent) leaf...
            let qcs = once((**qc).clone())
                // ...and each leaf in the chain justifies the subsequent leaf (its parent) through
                // `leaf.justify_qc`.
                .chain(leaf_chain.iter().map(|leaf| leaf.leaf.justify_qc()))
                // Put the QCs in chronological order.
                .rev()
                // The oldest QC is the `justify_qc` of the oldest leaf, which does not justify any
                // leaf in the new chain, so we don't need it.
                .skip(1);
            for (
                qc,
                LeafInfo {
                    leaf, vid_share, ..
                },
            ) in qcs.zip(leaf_chain.iter().rev())
            {
                let leaf_data =
                    LeafQueryData::new(leaf.clone(), qc.clone()).context("inconsistent leaf")?;
                self.insert_leaf(leaf_data.clone()).await?;

                if let Some(vid_share) = vid_share {
                    self.insert_vid(
                        VidCommonQueryData::new(
                            leaf.block_header().clone(),
                            vid_share.common.clone(),
                        ),
                        Some(vid_share.share.clone()),
                    )
                    .await?;
                } else if leaf.view_number().u64() == 0 {
                    // HotShot does not run VID in consensus for the genesis block. In this case,
                    // the block payload is guaranteed to always be empty, so VID isn't really
                    // necessary. But for consistency, we will still store the VID dispersal data,
                    // computing it ourselves based on the well-known genesis VID commitment.
                    store_genesis_vid(self, leaf).await;
                } else {
                    tracing::error!(
                        "VID info for block {} not available at decide",
                        leaf.block_header().block_number()
                    );
                }

                if let Some(block) = leaf.block_payload() {
                    self.insert_block(BlockQueryData::new(leaf.block_header().clone(), block))
                        .await?;
                } else {
                    tracing::error!(
                        "block {} not available at decide",
                        leaf.block_header().block_number()
                    );
                }
            }
        }
        Ok(())
    }
}

async fn store_genesis_vid<Types: NodeType>(
    storage: &mut impl UpdateAvailabilityData<Types>,
    leaf: &Leaf<Types>,
) where
    <Types as NodeType>::InstanceState: Default,
{
    let payload = Payload::<Types>::empty().0;
    let bytes = payload.encode();
    match vid_scheme(GENESIS_VID_NUM_STORAGE_NODES).disperse(bytes) {
        Ok(disperse) if disperse.commit != leaf.block_header().payload_commitment() => {
            tracing::error!(
                computed = %disperse.commit,
                header = %leaf.block_header().payload_commitment(),
                "computed VID commit for genesis block does not match header",
            );
        }
        Ok(mut disperse) => {
            if let Err(err) = storage
                .insert_vid(
                    VidCommonQueryData::new(leaf.block_header().clone(), disperse.common),
                    Some(disperse.shares.remove(0)),
                )
                .await
            {
                tracing::error!(%err, "unable to store genesis VID");
            }
        }
        Err(err) => {
            tracing::error!(%err, "unable to compute VID dispersal for genesis block");
        }
    }
}

/// A data source with an atomic transaction-based synchronization interface.
///
/// Changes are made to a versioned data source through a [`Transaction`]. Any changes made in a
/// [`Transaction`] are initially visible only when queried through that same [`Transaction`]. They
/// are not immediately written back to storage, which means that a new data source object opened
/// against the same persistent storage will not reflect the changes. In particular, this means that
/// if the process restarts and reopens its storage, uncommitted changes will be lost.
///
/// Only when a [`Transaction`] is committed are changes written back to storage, synchronized with
/// any concurrent changes, and made visible to other connections to the same data source.
pub trait VersionedDataSource: Send + Sync {
    /// A transaction which can read and modify the data source.
    type Transaction<'a>: Transaction
    where
        Self: 'a;

    type ReadOnly<'a>: Transaction
    where
        Self: 'a;

    /// Start an atomic transaction on the data source.
    fn write(&self) -> impl Future<Output = anyhow::Result<Self::Transaction<'_>>> + Send;

    /// Start a read-only transaction on the data source.
    ///
    /// A read-only transaction allows the owner to string together multiple queries of the data
    /// source, which otherwise would not be atomic with respect to concurrent writes, in an atomic
    /// fashion. Upon returning, [`read`](Self::read) locks in a fully consistent snapshot of the
    /// data source, and any read operations performed upon the transaction thereafter read from the
    /// same consistent snapshot. Concurrent modifications to the data source may occur (for
    /// example, from concurrent [`write`](Self::write) transactions being committed), but their
    /// results will not be reflected in a successful read-only transaction which was opened before
    /// the write was committed.
    ///
    /// Read-only transactions do not need to be committed, and reverting has no effect.
    fn read(&self) -> impl Future<Output = anyhow::Result<Self::ReadOnly<'_>>> + Send;
}

/// A unit of atomicity for updating a shared data sourec.
///
/// The methods provided by this trait can be used to write such pending changes back to persistent
/// storage ([commit](Self::commit)) so that they become visible to other clients of the same
/// underlying storage, and are saved if the process restarts. It also allows pending changes to be
/// rolled back ([revert](Self::revert)) so that they are never written back to storage and are no
/// longer reflected even through the data source object which was used to make the changes.
pub trait Transaction: Send + Sync {
    fn commit(self) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn revert(self) -> impl Future + Send;
}
