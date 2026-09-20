use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use pumpkin_util::math::position::BlockPos;
use rustc_hash::FxHashSet;

use crate::tick::{MAX_TICK_DELAY, OrderedTick, ScheduledTick};

pub struct ChunkTickScheduler<T> {
    inner: Mutex<Option<Box<ChunkTickSchedulerInner<T>>>>,
    offset: AtomicU64,
}

struct ChunkTickSchedulerInner<T> {
    tick_queue: [Vec<OrderedTick<T>>; MAX_TICK_DELAY],
    ready_ticks: Vec<OrderedTick<T>>,
    queued_ticks: FxHashSet<(BlockPos, T)>,
}

impl<'a, T: std::hash::Hash + Eq> ChunkTickScheduler<&'a T> {
    pub fn step_tick(&self) {
        let current_offset = self.offset.fetch_add(1, Ordering::SeqCst) as i64;
        let current_index = current_offset.rem_euclid(MAX_TICK_DELAY as i64) as usize;

        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_mut() else {
            return;
        };

        for tick in std::mem::take(&mut inner.tick_queue[current_index]) {
            if tick.deadline <= current_offset {
                inner.ready_ticks.push(tick);
            } else {
                inner.tick_queue[current_index].push(tick);
            }
        }
    }

    pub fn take_ready_ticks(&self) -> Vec<OrderedTick<&'a T>> {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_mut() else {
            return Vec::new();
        };

        inner.ready_ticks.sort_unstable_by(|left, right| {
            left.deadline
                .cmp(&right.deadline)
                .then_with(|| left.priority.cmp(&right.priority))
                .then_with(|| left.sub_tick_order.cmp(&right.sub_tick_order))
        });
        let ready_ticks = std::mem::take(&mut inner.ready_ticks);
        for tick in &ready_ticks {
            inner.queued_ticks.remove(&(tick.position, tick.value));
        }
        if inner.queued_ticks.is_empty() {
            *inner_guard = None;
        }
        ready_ticks
    }

    pub fn schedule_tick(&self, tick: &ScheduledTick<&'a T>, sub_tick_order: u64) {
        let offset = self.offset.load(Ordering::SeqCst) as i64;
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = inner_guard.get_or_insert_with(|| {
            Box::new(ChunkTickSchedulerInner {
                tick_queue: std::array::from_fn(|_| Vec::new()),
                ready_ticks: Vec::new(),
                queued_ticks: FxHashSet::default(),
            })
        });

        if inner.queued_ticks.insert((tick.position, tick.value)) {
            let deadline = offset + i64::from(tick.delay);
            let index = if deadline <= offset {
                offset.rem_euclid(MAX_TICK_DELAY as i64) as usize
            } else {
                deadline.rem_euclid(MAX_TICK_DELAY as i64) as usize
            };

            inner.tick_queue[index].push(OrderedTick {
                deadline,
                priority: tick.priority,
                sub_tick_order,
                position: tick.position,
                value: tick.value,
            });
        }
    }

    pub fn is_scheduled(&self, pos: BlockPos, value: &T) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|inner| inner.queued_ticks.contains(&(pos, value)))
    }

    pub fn clear_area(&self, min: &BlockPos, max: &BlockPos) {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_mut() else {
            return;
        };

        let contains = |position: &BlockPos| {
            position.0.x >= min.0.x
                && position.0.x < max.0.x
                && position.0.y >= min.0.y
                && position.0.y < max.0.y
                && position.0.z >= min.0.z
                && position.0.z < max.0.z
        };

        for queue in &mut inner.tick_queue {
            queue.retain(|tick| !contains(&tick.position));
        }
        inner.ready_ticks.retain(|tick| !contains(&tick.position));
        inner
            .queued_ticks
            .retain(|(position, _)| !contains(position));
        let became_empty = inner.queued_ticks.is_empty();

        if became_empty {
            *inner_guard = None;
        }
    }

    pub fn has_ticks(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|inner| !inner.queued_ticks.is_empty())
    }

    #[must_use]
    pub fn to_vec(&self) -> Vec<ScheduledTick<&'a T>> {
        let offset = self.offset.load(Ordering::SeqCst) as i64;
        let inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_ref() else {
            return Vec::new();
        };

        let mut res = inner
            .ready_ticks
            .iter()
            .map(|x| ScheduledTick {
                delay: (x.deadline - offset) as i32,
                priority: x.priority,
                position: x.position,
                value: x.value,
            })
            .collect::<Vec<_>>();

        for queue in &inner.tick_queue {
            res.extend(queue.iter().map(|x| ScheduledTick {
                delay: (x.deadline - offset) as i32,
                priority: x.priority,
                position: x.position,
                value: x.value,
            }));
        }
        res
    }
}

impl<'a, T: std::hash::Hash + Eq + 'static> FromIterator<ScheduledTick<&'a T>>
    for ChunkTickScheduler<&'a T>
{
    fn from_iter<I: IntoIterator<Item = ScheduledTick<&'a T>>>(iter: I) -> Self {
        let scheduler = Self::default();
        let iter = iter.into_iter();

        let (lower, _) = iter.size_hint();
        if lower > 0 {
            let mut inner_guard = scheduler
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let inner = inner_guard.get_or_insert_with(|| {
                Box::new(ChunkTickSchedulerInner {
                    tick_queue: std::array::from_fn(|_| Vec::new()),
                    ready_ticks: Vec::new(),
                    queued_ticks: FxHashSet::default(),
                })
            });
            inner.queued_ticks.reserve(lower);
        }

        for (sub_tick_order, tick) in iter.enumerate() {
            scheduler.schedule_tick(&tick, sub_tick_order as u64);
        }
        scheduler
    }
}

impl<T> Default for ChunkTickScheduler<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
            offset: AtomicU64::new(0),
        }
    }
}
