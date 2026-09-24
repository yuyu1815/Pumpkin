use crate::block::blocks::falling::FallingBlock;
use crate::block::registry::BlockActionResult;
use crate::block::{
    AttackArgs, BlockBehaviour, NormalUseArgs, OnScheduledTickArgs, PathComputationType, PlacedArgs,
};
use crate::world::World;
use pumpkin_data::BlockState;
use pumpkin_macros::pumpkin_block;
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::tick::TickPriority;
use rand::{RngExt, rng};
use std::sync::Arc;

#[pumpkin_block("minecraft:dragon_egg")]
pub struct DragonEggBlock;

impl DragonEggBlock {
    fn teleport(world: &Arc<World>, pos: &BlockPos) {
        Self::relocate(world, pos, Self::random_candidates(pos));
    }

    fn random_candidates(pos: &BlockPos) -> impl Iterator<Item = BlockPos> + '_ {
        std::iter::repeat_with(move || {
            BlockPos::new(
                pos.0.x + rng().random_range(-16..16),
                pos.0.y + rng().random_range(-8..8),
                pos.0.z + rng().random_range(-16..16),
            )
        })
        .take(1000)
    }

    fn relocate(
        world: &Arc<World>,
        pos: &BlockPos,
        candidates: impl IntoIterator<Item = BlockPos>,
    ) -> bool {
        for test_pos in candidates {
            let state = world.get_block_state(&test_pos);
            let below_state = world.get_block_state(&test_pos.down());

            if state.is_air() && !below_state.is_air() {
                if world.get_block(pos) != &pumpkin_data::Block::DRAGON_EGG {
                    return false;
                }
                let current_state = world.get_block_state(pos);
                world.set_block_state(
                    &test_pos,
                    current_state.id,
                    pumpkin_world::world::BlockFlags::NOTIFY_ALL,
                );
                world.set_block_state(
                    pos,
                    pumpkin_data::Block::AIR.default_state.id,
                    pumpkin_world::world::BlockFlags::NOTIFY_ALL,
                );
                return true;
            }
        }
        false
    }
}

impl DragonEggBlock {
    fn on_attack_with_candidates(
        world: &Arc<World>,
        position: &BlockPos,
        candidates: impl IntoIterator<Item = BlockPos>,
    ) -> bool {
        Self::relocate(world, position, candidates);
        true
    }
}

impl BlockBehaviour for DragonEggBlock {
    fn placed(&self, args: PlacedArgs<'_>) {
        args.world
            .schedule_block_tick(args.block, *args.position, 5, TickPriority::Normal);
    }

    fn normal_use(&self, args: NormalUseArgs<'_>) -> BlockActionResult {
        Self::teleport(args.world, args.position);
        BlockActionResult::Success
    }

    fn on_attack(&self, args: AttackArgs<'_>) -> bool {
        Self::on_attack_with_candidates(
            args.world,
            args.position,
            Self::random_candidates(args.position),
        )
    }

    fn on_scheduled_tick(&self, args: OnScheduledTickArgs<'_>) {
        FallingBlock::on_scheduled_tick(&FallingBlock, args);
    }

    fn is_pathfindable(&self, _state: &BlockState, _computation_type: PathComputationType) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak};

    use arc_swap::ArcSwap;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_data::{Block, dimension::Dimension};
    use pumpkin_util::{
        math::{position::BlockPos, vector2::Vector2},
        world_seed::Seed,
    };
    use pumpkin_world::{chunk::ChunkData, level::Level, world::BlockFlags, world_info::LevelData};
    use tempfile::TempDir;

    use super::*;

    fn test_world(root: &std::path::Path) -> Arc<World> {
        let level = Level::from_root_folder(
            &LevelConfig {
                autosave_ticks: 0,
                ..LevelConfig::default()
            },
            root.to_path_buf(),
            0,
            Dimension::OVERWORLD,
        );
        let world = Arc::new(World::load(
            level,
            Arc::new(ArcSwap::new(Arc::new(LevelData::default(Seed(0))))),
            Dimension::OVERWORLD,
            crate::block::registry::default_registry(),
            Weak::new(),
        ));
        world
            .level
            .loaded_chunks
            .insert(Vector2::new(0, 0), ChunkData::empty_sync(0, 0));
        world
    }

    fn prepare_egg(world: &Arc<World>) -> (BlockPos, BlockPos) {
        let origin = BlockPos::new(0, 64, 0);
        let destination = BlockPos::new(2, 64, 2);
        world.set_block_state(
            &origin,
            Block::DRAGON_EGG.default_state.id,
            BlockFlags::empty(),
        );
        world.set_block_state(
            &destination.down(),
            Block::STONE.default_state.id,
            BlockFlags::empty(),
        );
        (origin, destination)
    }

    #[tokio::test]
    async fn attack_relocates_exactly_one_egg_and_suppresses_breaking() {
        let temp_dir = TempDir::new().unwrap();
        let world = test_world(temp_dir.path());
        let (origin, destination) = prepare_egg(&world);

        assert!(DragonEggBlock::on_attack_with_candidates(
            &world,
            &origin,
            [destination],
        ));
        assert_eq!(world.get_block(&origin), &Block::AIR);
        assert_eq!(world.get_block(&destination), &Block::DRAGON_EGG);
    }

    #[tokio::test]
    async fn attack_without_destination_keeps_original_egg() {
        let temp_dir = TempDir::new().unwrap();
        let world = test_world(temp_dir.path());
        let (origin, blocked_candidate) = prepare_egg(&world);
        world.set_block_state(
            &blocked_candidate,
            Block::STONE.default_state.id,
            BlockFlags::empty(),
        );

        assert!(DragonEggBlock::on_attack_with_candidates(
            &world,
            &origin,
            [blocked_candidate],
        ));
        assert_eq!(world.get_block(&origin), &Block::DRAGON_EGG);
    }

    #[tokio::test]
    async fn attack_does_not_relocate_a_replaced_source_block() {
        let temp_dir = TempDir::new().unwrap();
        let world = test_world(temp_dir.path());
        let (origin, destination) = prepare_egg(&world);
        world.set_block_state(&origin, Block::STONE.default_state.id, BlockFlags::empty());

        assert!(DragonEggBlock::on_attack_with_candidates(
            &world,
            &origin,
            [destination],
        ));
        assert_eq!(world.get_block(&origin), &Block::STONE);
        assert_eq!(world.get_block(&destination), &Block::AIR);
    }
}
