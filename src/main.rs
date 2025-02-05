use crate::distance::Distance;
use crate::post::PostEndpoint;
use config::Config;
use eth2::{
    types::{BlindedBeaconBlock, BlockId, Slot},
    BeaconNodeHttpClient, Timeouts,
};
use eth2_network_config::Eth2NetworkConfig;
use futures::future::join_all;
use itertools::Itertools;
use node::Node;
use sensitive_url::SensitiveUrl;
use slot_clock::{SlotClock, SystemTimeSlotClock};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

mod config;
mod distance;
mod node;
mod post;
mod tests;

#[cfg(all(feature = "mainnet", not(feature = "gnosis")))]
type E = eth2::types::MainnetEthSpec;
#[cfg(feature = "gnosis")]
type E = eth2::types::GnosisEthSpec;

// FIXME: add to config
const VERBOSE: bool = false;

const SIGNIFICANCE_NUMERATOR: usize = 2;
const SIGNIFICANCE_DENOM: usize = 1;
const NUM_SLOTS_IN_MEMORY: u64 = 8;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    run().await.unwrap();
}

async fn run() -> Result<(), String> {
    // Load config.
    let config = Config::from_file(Path::new("config.toml")).unwrap();
    eprintln!("{:#?}", config);

    // Mapping from node name to label.
    let labels = config
        .nodes
        .iter()
        .map(|node| (node.name.clone(), node.label.clone()))
        .collect::<HashMap<_, _>>();

    // Get network config and slot clock.
    let network_config = match (&config.network, &config.network_dir) {
        (Some(network), None) => Eth2NetworkConfig::constant(network)?
            .ok_or_else(|| format!("Unknown network `{}`", network))?,
        (None, Some(network_dir)) => Eth2NetworkConfig::load(network_dir.clone())?,
        (Some(_), Some(_)) => return Err("conflicting network and network_dir".into()),
        (None, None) => return Err("one of network or network_dir is required".into()),
    };
    let spec = network_config.chain_spec::<E>()?;
    let genesis_state = network_config.beacon_state::<E>()?;
    let slot_clock = SystemTimeSlotClock::new(
        spec.genesis_slot,
        Duration::from_secs(genesis_state.genesis_time()),
        Duration::from_secs(spec.seconds_per_slot),
    );

    // Establish connections to beacon nodes.
    let nodes = config
        .nodes
        .iter()
        .cloned()
        .map(|config| Node::new(config))
        .collect::<Result<Vec<_>, String>>()?;

    // Establish connection to canonical BN.
    let canonical_bn = {
        let url = SensitiveUrl::parse(&config.canonical_bn)
            .map_err(|e| format!("Invalid URL: {:?}", e))?;
        BeaconNodeHttpClient::new(url, Timeouts::set_all(Duration::from_secs(6)))
    };

    // Establish connection to post endpoint.
    let post_endpoint = PostEndpoint::new(&config);

    // Main loop.
    let mut all_blocks: HashMap<Slot, HashMap<String, BlindedBeaconBlock<E>>> = HashMap::new();

    loop {
        let wait = slot_clock.duration_to_next_slot().expect("post genesis");
        tokio::time::sleep(wait).await;

        let slot = slot_clock.now().unwrap();

        // Dispatch requests in parallel to all dreaming nodes.
        let handles = nodes
            .iter()
            .map(|node| {
                let inner = node.clone();
                let slot_clock = slot_clock.clone();
                let name = node.config.name.clone();

                tokio::spawn(async move {
                    let current_slot = slot_clock.now().unwrap();
                    if current_slot != slot {
                        return Err(format!(
                            "too slow, slot {} expired (slot now: {})",
                            slot, current_slot
                        ));
                    }
                    let slot_offset = slot_clock
                        .seconds_from_current_slot_start(spec.seconds_per_slot)
                        .unwrap();
                    if VERBOSE {
                        eprintln!(
                            "requesting block from {} at {}s after slot start",
                            name,
                            slot_offset.as_secs()
                        );
                    }

                    let blinded_block = if inner.config.use_builder {
                        inner.get_blinded_block_with_timeout::<E>(slot).await?
                    } else {
                        let full_block = inner.get_block_with_timeout::<E>(slot).await?;
                        let (blinded_block, _payload) = full_block.into();
                        blinded_block
                    };
                    Ok(blinded_block)
                })
            })
            .collect::<Vec<_>>();

        let mut slot_blocks = HashMap::new();
        let mut post_blocks = vec![];

        for (result, node) in join_all(handles).await.into_iter().zip(&nodes) {
            let name = node.config.name.clone();

            let block = match result.map_err(|e| format!("Task panicked: {:?}", e))? {
                Ok(block) => block,
                Err(e) => {
                    eprintln!("{} failed to produce a block: {}", name, e);
                    continue;
                }
            };
            eprintln!(
                "slot {}: block from {} with {} attestations",
                slot,
                name,
                block.body().attestations().len()
            );

            if post_endpoint.is_some() {
                post_blocks.push(block.clone());
            }

            slot_blocks.insert(node.config.name.clone(), block);
        }

        if let Some(ref post_endpoint) = post_endpoint {
            let names_and_labels = nodes
                .iter()
                .map(|node| (node.config.name.clone(), node.config.label.clone()))
                .collect_vec();
            let endpoint = post_endpoint.clone();
            tokio::spawn(async move {
                if let Err(e) = endpoint
                    .post_blocks(names_and_labels, post_blocks, slot)
                    .await
                {
                    eprintln!("error posting blocks at slot {}: {}", slot, e);
                }
            });
        }

        if slot_blocks.len() == nodes.len() {
            all_blocks.insert(slot, slot_blocks);
        } else {
            eprintln!("slot {slot}: discarding results due to failures");
        }

        // Compare canonical block from previous slot to dream blocks.
        let prev_slot = slot - 1;
        match canonical_bn
            .get_beacon_blocks(BlockId::Slot(prev_slot))
            .await
        {
            Ok(Some(res)) => {
                let (full_block, _) = res.data.deconstruct();
                let (block, _) = full_block.into();
                if let Some(dream_blocks) = all_blocks.get(&prev_slot) {
                    let mut distances = dream_blocks
                        .iter()
                        .map(|(name, dream_block)| {
                            let delta = dream_block.delta(&block).unwrap();
                            let distance = BlindedBeaconBlock::<E>::delta_to_distance(&delta);
                            if VERBOSE {
                                eprintln!("canonical({})-{} delta: {:#?}", prev_slot, name, delta);
                            }
                            eprintln!(
                                "slot {}: canonical <=> {} distance: {}",
                                prev_slot, name, distance
                            );
                            (name, distance)
                        })
                        .collect::<Vec<_>>();

                    distances.sort_unstable_by_key(|(_, distance)| *distance);

                    let (closest_name, closest_distance) = &distances[0];
                    let (second_closest_name, second_closest_distance) =
                        &distances.get(1).unwrap_or(&distances[0]);

                    let closest_label = &labels[closest_name.as_str()];
                    let second_closest_label = &labels[second_closest_name.as_str()];

                    if closest_label == second_closest_label {
                        eprintln!(
                            "slot {}: canonical block is likely {}@{} (two closest match)",
                            prev_slot, closest_label, closest_distance
                        );
                    } else if *second_closest_distance
                        >= closest_distance * SIGNIFICANCE_NUMERATOR / SIGNIFICANCE_DENOM
                    {
                        eprintln!(
                            "slot {}: canonical block is likely {} \
                             (significantly closer @{} than 2nd place {}@{})",
                            prev_slot,
                            closest_label,
                            closest_distance,
                            second_closest_label,
                            second_closest_distance
                        );
                    } else {
                        eprintln!(
                            "slot {}: canonical block is too close to call ({}@{} vs {}@{})",
                            prev_slot,
                            closest_name,
                            closest_distance,
                            second_closest_name,
                            second_closest_distance
                        );
                    }
                } else {
                    eprintln!("No dream blocks for slot {}", prev_slot);
                }
            }
            Ok(None) => {
                eprintln!("No canonical block at slot {}", prev_slot);
            }
            Err(e) => {
                eprintln!(
                    "Error fetching canonical block at slot {}: {:?}",
                    prev_slot, e
                );
            }
        }

        if let Some(blocks) = all_blocks.get(&slot) {
            for (name1, block1) in blocks {
                for (name2, block2) in blocks {
                    // Use lexicographic name ordering to establish order.
                    if name1 >= name2 {
                        continue;
                    }

                    let delta = block1.delta(block2).unwrap();
                    if VERBOSE {
                        eprintln!("{}-{} delta: {:#?}", name1, name2, delta);
                    }
                    eprintln!(
                        "slot {}: {} <=> {} distance: {}",
                        slot,
                        name1,
                        name2,
                        BlindedBeaconBlock::<E>::delta_to_distance(&delta)
                    );
                }
            }
        }

        // Prune blocks to prevent the in-memory map from consuming too much memory. We really only
        // need the 2 most recent slots, but there's no harm in keeping a few more.
        all_blocks.retain(|stored_slot, _| *stored_slot + NUM_SLOTS_IN_MEMORY >= slot);
    }
}
