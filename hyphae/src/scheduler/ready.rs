use rustc_hash::FxHashMap;
use uuid::Uuid;

use super::frontier::Frontier;

pub(super) type PendingRun = Box<dyn FnOnce() + Send>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManyHandle {
    index: usize,
    generation: u64,
}

struct ManySlot {
    generation: u64,
    group: Option<ManyGroup>,
}

#[derive(Default)]
struct ManyArena {
    slots: Vec<ManySlot>,
    free: Vec<usize>,
}

impl ManyArena {
    fn insert(&mut self, group: ManyGroup) -> ManyHandle {
        if let Some(index) = self.free.pop()
            && let Some(slot) = self.slots.get_mut(index)
        {
            slot.group = Some(group);
            return ManyHandle {
                index,
                generation: slot.generation,
            };
        }

        let index = self.slots.len();
        self.slots.push(ManySlot {
            generation: 1,
            group: Some(group),
        });
        ManyHandle {
            index,
            generation: 1,
        }
    }

    fn get_mut(&mut self, handle: ManyHandle) -> Option<&mut ManyGroup> {
        let slot = self.slots.get_mut(handle.index)?;
        (slot.generation == handle.generation)
            .then_some(())
            .and(slot.group.as_mut())
    }

    fn take(&mut self, handle: ManyHandle) -> Option<ManyGroup> {
        let slot = self.slots.get_mut(handle.index)?;
        if slot.generation != handle.generation {
            return None;
        }
        let group = slot.group.take()?;
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
            self.free.push(handle.index);
        }
        Some(group)
    }
}

struct ManyGroup {
    runs: Vec<Option<PendingRun>>,
    live: usize,
    scheduled: Option<usize>,
}

impl ManyGroup {
    fn from_pair(
        first: PendingRun,
        first_scheduled: bool,
        second: PendingRun,
        second_scheduled: bool,
    ) -> Self {
        Self {
            runs: vec![Some(first), Some(second)],
            live: 2,
            scheduled: if second_scheduled {
                Some(1)
            } else {
                first_scheduled.then_some(0)
            },
        }
    }

    fn push(&mut self, run: PendingRun, scheduled: bool) {
        let position = self.runs.len();
        self.runs.push(Some(run));
        self.live = self.live.saturating_add(1);
        if scheduled {
            self.scheduled = Some(position);
        }
    }

    fn replace_scheduled(&mut self, run: PendingRun) -> Result<PendingRun, PendingRun> {
        let Some(position) = self.scheduled else {
            return Err(run);
        };
        let is_tail = self
            .runs
            .get(position.saturating_add(1)..)
            .is_some_and(|tail| tail.iter().all(Option::is_none));
        if is_tail {
            let Some(slot) = self.runs.get_mut(position) else {
                return Err(run);
            };
            let Some(displaced) = slot.take() else {
                return Err(run);
            };
            *slot = Some(run);
            return Ok(displaced);
        }

        let Some(displaced) = self.runs.get_mut(position).and_then(Option::take) else {
            return Err(run);
        };
        self.live = self.live.saturating_sub(1);
        self.push(run, true);
        self.compact_if_needed();
        Ok(displaced)
    }

    fn take_scheduled(&mut self) -> Option<PendingRun> {
        let position = self.scheduled.take()?;
        let run = self.runs.get_mut(position)?.take()?;
        self.live = self.live.saturating_sub(1);
        Some(run)
    }

    fn compact_if_needed(&mut self) {
        let threshold = self.live.saturating_mul(2).saturating_add(8);
        if self.runs.len() <= threshold {
            return;
        }
        let old_scheduled = self.scheduled;
        let mut compact = Vec::with_capacity(self.live);
        let mut scheduled = None;
        for (old_position, run) in self.runs.drain(..).enumerate() {
            if let Some(run) = run {
                if old_scheduled == Some(old_position) {
                    scheduled = Some(compact.len());
                }
                compact.push(Some(run));
            }
        }
        self.runs = compact;
        self.scheduled = scheduled;
    }

    fn into_runs(self) -> Vec<PendingRun> {
        self.runs.into_iter().flatten().collect()
    }

    fn into_single(mut self) -> Option<PendingRun> {
        self.runs.iter_mut().find_map(Option::take)
    }
}

enum Group {
    One(PendingRun),
    Many(Box<ManyHandle>),
}

pub(super) struct ReadyQueue {
    frontier: Frontier<(u64, Uuid), Group>,
    scheduled: FxHashMap<Uuid, u64>,
    many: Option<Box<ManyArena>>,
}

impl ReadyQueue {
    pub(super) fn new() -> Self {
        Self {
            frontier: Frontier::new(),
            scheduled: FxHashMap::default(),
            many: None,
        }
    }

    /// Queue one operation and return a superseded closure for destruction
    /// after the caller releases the scheduler mutex.
    #[inline]
    pub(super) fn enqueue(
        &mut self,
        id: Uuid,
        height: u64,
        coalesce: bool,
        run: PendingRun,
    ) -> Option<PendingRun> {
        if coalesce {
            let previous = self.scheduled.insert(id, height);
            let key = (height, id);
            if previous.is_none() {
                if let Some(existing) = self.frontier.insert(key, Group::One(run)) {
                    self.merge_replaced(key, existing, false, true);
                }
                return None;
            }
            if previous == Some(height) {
                let run = match self.frontier.get_mut(&key) {
                    Some(Group::One(current)) => return Some(std::mem::replace(current, run)),
                    Some(Group::Many(handle)) => match self
                        .many
                        .as_deref_mut()
                        .and_then(|many| many.get_mut(**handle))
                    {
                        Some(many) => match many.replace_scheduled(run) {
                            Ok(displaced) => return Some(displaced),
                            Err(run) => run,
                        },
                        None => run,
                    },
                    None => run,
                };
                self.append(id, height, true, run, false);
                return None;
            }

            let displaced = previous.and_then(|old_height| self.remove_at(id, old_height));
            self.append(id, height, true, run, false);
            return displaced;
        }

        let old_was_scheduled = self.scheduled.get(&id) == Some(&height);
        self.append(id, height, false, run, old_was_scheduled);
        None
    }

    #[cold]
    fn merge_replaced(
        &mut self,
        key: (u64, Uuid),
        existing: Group,
        old_was_scheduled: bool,
        coalesce: bool,
    ) {
        match self.frontier.remove(&key) {
            Some(Group::One(run)) => {
                self.append_existing(key, existing, run, old_was_scheduled, coalesce);
            }
            Some(replacement) => {
                self.frontier.insert(key, replacement);
            }
            None => {
                self.frontier.insert(key, existing);
            }
        }
    }

    #[cold]
    fn append(
        &mut self,
        id: Uuid,
        height: u64,
        coalesce: bool,
        run: PendingRun,
        old_was_scheduled: bool,
    ) {
        let key = (height, id);
        if let Some(Group::Many(handle)) = self.frontier.get_mut(&key) {
            let handle = **handle;
            if let Some(many) = self
                .many
                .as_deref_mut()
                .and_then(|arena| arena.get_mut(handle))
            {
                many.push(run, coalesce);
                return;
            }
        }
        if let Some(existing) = self.frontier.insert(key, Group::One(run)) {
            self.merge_replaced(key, existing, old_was_scheduled, coalesce);
        }
    }

    #[cold]
    fn append_existing(
        &mut self,
        key: (u64, Uuid),
        existing: Group,
        run: PendingRun,
        old_was_scheduled: bool,
        coalesce: bool,
    ) {
        let group = match existing {
            Group::One(first) => {
                let many = ManyGroup::from_pair(first, old_was_scheduled, run, coalesce);
                Group::Many(Box::new(self.many_mut().insert(many)))
            }
            Group::Many(handle) => {
                if let Some(many) = self
                    .many
                    .as_deref_mut()
                    .and_then(|arena| arena.get_mut(*handle))
                {
                    many.push(run, coalesce);
                }
                Group::Many(handle)
            }
        };
        self.frontier.insert(key, group);
    }

    #[cold]
    fn remove_at(&mut self, id: Uuid, height: u64) -> Option<PendingRun> {
        let key = (height, id);
        match self.frontier.remove(&key)? {
            Group::One(run) => Some(run),
            Group::Many(handle) => {
                let mut many = self.many.as_deref_mut()?.take(*handle)?;
                let displaced = many.take_scheduled()?;
                if many.live == 1 {
                    if let Some(run) = many.into_single() {
                        self.frontier.insert(key, Group::One(run));
                    }
                } else if many.live > 1 {
                    let mut handle = handle;
                    *handle = self.many_mut().insert(many);
                    self.frontier.insert(key, Group::Many(handle));
                }
                Some(displaced)
            }
        }
    }

    #[cold]
    fn many_mut(&mut self) -> &mut ManyArena {
        self.many
            .get_or_insert_with(|| Box::new(ManyArena::default()))
    }

    #[inline]
    pub(super) fn pop_min_height_groups(&mut self) -> Option<Vec<Vec<PendingRun>>> {
        let target = self
            .frontier
            .first_key_value()
            .map(|(&(height, _), _)| height)?;
        let mut groups = Vec::new();

        while self
            .frontier
            .first_key_value()
            .is_some_and(|(&(height, _), _)| height == target)
        {
            let Some(((height, id), group)) = self.frontier.pop_first() else {
                break;
            };
            if self.scheduled.get(&id) == Some(&height) {
                self.scheduled.remove(&id);
            }
            let runs = match group {
                Group::One(run) => vec![run],
                Group::Many(handle) => self
                    .many
                    .as_deref_mut()
                    .and_then(|many| many.take(*handle))
                    .map(ManyGroup::into_runs)
                    .unwrap_or_default(),
            };
            if !runs.is_empty() {
                groups.push(runs);
            }
        }

        (!groups.is_empty()).then_some(groups)
    }

    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.frontier.is_empty()
    }

    #[cfg(test)]
    fn assert_consistent(&self) {
        use std::collections::HashSet;

        let mut referenced = HashSet::new();
        self.frontier.for_each(|(height, id), group| match group {
            Group::One(_) => {}
            Group::Many(handle) => {
                assert!(referenced.insert((handle.index, handle.generation)));
                let slot = self
                    .many
                    .as_ref()
                    .and_then(|arena| arena.slots.get(handle.index));
                assert!(slot.is_some());
                let Some(slot) = slot else {
                    return;
                };
                assert_eq!(slot.generation, handle.generation);
                let group = slot.group.as_ref();
                assert!(group.is_some());
                let Some(group) = group else {
                    return;
                };
                assert_eq!(
                    group.live,
                    group.runs.iter().filter(|run| run.is_some()).count()
                );
                if let Some(position) = group.scheduled {
                    assert!(group.runs.get(position).is_some_and(Option::is_some));
                    assert_eq!(self.scheduled.get(id), Some(height));
                }
            }
        });

        if let Some(arena) = self.many.as_ref() {
            let mut free = HashSet::new();
            for &index in &arena.free {
                assert!(free.insert(index));
                let slot = arena.slots.get(index);
                assert!(slot.is_some());
                if let Some(slot) = slot {
                    assert!(slot.group.is_none());
                }
            }
            for (index, slot) in arena.slots.iter().enumerate() {
                if slot.group.is_some() {
                    assert!(referenced.contains(&(index, slot.generation)));
                    assert!(!free.contains(&index));
                }
            }
        } else {
            assert!(referenced.is_empty());
        }

        for (id, height) in &self.scheduled {
            let group = self.frontier.get(&(*height, *id));
            assert!(group.is_some());
            match group {
                Some(Group::Many(handle)) => {
                    let many = self
                        .many
                        .as_ref()
                        .and_then(|arena| arena.slots.get(handle.index))
                        .and_then(|slot| slot.group.as_ref());
                    assert!(many.and_then(|group| group.scheduled).is_some());
                }
                Some(Group::One(_)) | None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use super::*;

    fn recorded(value: u8, seen: &Arc<Mutex<Vec<u8>>>) -> PendingRun {
        let seen = Arc::clone(seen);
        Box::new(move || {
            if let Ok(mut seen) = seen.lock() {
                seen.push(value);
            }
        })
    }

    fn run(groups: Vec<Vec<PendingRun>>) {
        for group in groups {
            for op in group {
                op();
            }
        }
    }

    fn snapshot(seen: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
        match seen.lock() {
            Ok(seen) => seen.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    struct ReferenceQueue {
        order: BTreeMap<(u64, Uuid, u64), u16>,
        scheduled: FxHashMap<Uuid, (u64, u64)>,
        sequence: u64,
    }

    impl ReferenceQueue {
        fn new() -> Self {
            Self {
                order: BTreeMap::new(),
                scheduled: FxHashMap::default(),
                sequence: 0,
            }
        }

        fn enqueue(&mut self, id: Uuid, height: u64, coalesce: bool, payload: u16) {
            let sequence = self.sequence;
            self.sequence = self.sequence.saturating_add(1);
            if coalesce
                && let Some((old_height, old_sequence)) =
                    self.scheduled.insert(id, (height, sequence))
            {
                self.order.remove(&(old_height, id, old_sequence));
            }
            self.order.insert((height, id, sequence), payload);
        }

        fn pop(&mut self) -> Option<Vec<u16>> {
            let target = self
                .order
                .first_key_value()
                .map(|(&(height, _, _), _)| height)?;
            let mut payloads = Vec::new();
            while self
                .order
                .first_key_value()
                .is_some_and(|(&(height, _, _), _)| height == target)
            {
                let Some(((_, id, sequence), payload)) = self.order.pop_first() else {
                    break;
                };
                if self
                    .scheduled
                    .get(&id)
                    .is_some_and(|&(_, live_sequence)| live_sequence == sequence)
                {
                    self.scheduled.remove(&id);
                }
                payloads.push(payload);
            }
            Some(payloads)
        }
    }

    fn recorded_u16(value: u16, seen: &Arc<Mutex<Vec<u16>>>) -> PendingRun {
        let seen = Arc::clone(seen);
        Box::new(move || {
            if let Ok(mut seen) = seen.lock() {
                seen.push(value);
            }
        })
    }

    fn drain_u16(seen: &Arc<Mutex<Vec<u16>>>) -> Vec<u16> {
        match seen.lock() {
            Ok(mut seen) => std::mem::take(&mut *seen),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }

    #[test]
    fn common_frontier_value_stays_compact() {
        let words = std::mem::size_of::<usize>();
        assert_eq!(std::mem::size_of::<PendingRun>(), words.saturating_mul(2));
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<ManyHandle>(), 16);
            assert_eq!(std::mem::size_of::<Group>(), 16);
            assert_eq!(std::mem::size_of::<ReadyQueue>(), 72);
        }
    }

    #[test]
    fn coalescing_replaces_the_old_operation() {
        let id = Uuid::new_v4();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        assert!(queue.enqueue(id, 2, true, recorded(1, &seen)).is_none());
        let displaced = queue.enqueue(id, 2, true, recorded(2, &seen));
        drop(displaced);
        assert_eq!(queue.frontier.len(), 1);
        assert!(matches!(queue.frontier.get(&(2, id)), Some(Group::One(_))));
        let groups = queue.pop_min_height_groups();
        assert!(groups.is_some(), "coalesced group was missing");
        let Some(groups) = groups else {
            return;
        };
        run(groups);
        assert_eq!(snapshot(&seen), vec![2]);
        assert!(queue.is_empty());
    }

    #[test]
    fn event_then_two_values_tracks_the_promoted_value() {
        let id = Uuid::new_v4();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        assert!(queue.enqueue(id, 1, false, recorded(1, &seen)).is_none());
        assert!(queue.enqueue(id, 1, true, recorded(2, &seen)).is_none());
        drop(queue.enqueue(id, 1, true, recorded(3, &seen)));
        let Some(groups) = queue.pop_min_height_groups() else {
            return;
        };
        run(groups);
        assert_eq!(snapshot(&seen), vec![1, 3]);
    }

    #[test]
    fn terminal_between_values_keeps_arrival_order() {
        let id = Uuid::new_v4();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        assert!(queue.enqueue(id, 1, true, recorded(1, &seen)).is_none());
        assert!(queue.enqueue(id, 1, false, recorded(2, &seen)).is_none());
        let displaced = queue.enqueue(id, 1, true, recorded(3, &seen));
        drop(displaced);
        let groups = queue.pop_min_height_groups();
        assert!(groups.is_some(), "mixed group was missing");
        let Some(groups) = groups else {
            return;
        };
        run(groups);
        assert_eq!(snapshot(&seen), vec![2, 3]);
    }

    #[test]
    fn height_change_removes_an_empty_old_group() {
        let id = Uuid::new_v4();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        assert!(queue.enqueue(id, 8, true, recorded(8, &seen)).is_none());
        let displaced = queue.enqueue(id, 3, true, recorded(3, &seen));
        drop(displaced);
        let groups = queue.pop_min_height_groups();
        assert!(groups.is_some(), "replacement group was missing");
        let Some(groups) = groups else {
            return;
        };
        run(groups);
        assert_eq!(snapshot(&seen), vec![3]);
        assert!(queue.is_empty());
    }

    #[test]
    fn noncoalescing_events_share_one_ordered_group() {
        let id = Uuid::new_v4();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        for value in 0..16 {
            assert!(
                queue
                    .enqueue(id, 4, false, recorded(value, &seen))
                    .is_none()
            );
        }
        assert_eq!(queue.frontier.len(), 1);
        let groups = queue.pop_min_height_groups();
        assert!(groups.is_some(), "event group was missing");
        let Some(groups) = groups else {
            return;
        };
        assert_eq!(groups.len(), 1);
        run(groups);
        assert_eq!(snapshot(&seen), (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn mixed_group_compacts_superseded_handles() {
        let id = Uuid::new_v4();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        assert!(queue.enqueue(id, 5, false, recorded(1, &seen)).is_none());
        for value in 2..=200 {
            let displaced = queue.enqueue(id, 5, true, recorded(value, &seen));
            drop(displaced);
        }
        let group = queue.frontier.get(&(5, id));
        assert!(group.is_some(), "mixed group was missing");
        let Some(group) = group else {
            return;
        };
        let Group::Many(handle) = group else {
            return;
        };
        let many = queue
            .many
            .as_deref_mut()
            .and_then(|arena| arena.get_mut(**handle));
        assert!(many.is_some(), "mixed arena group was missing");
        let Some(many) = many else {
            return;
        };
        assert_eq!(many.live, 2);
        assert!(many.runs.len() <= 12);

        let groups = queue.pop_min_height_groups();
        assert!(groups.is_some(), "mixed group did not drain");
        let Some(groups) = groups else {
            return;
        };
        run(groups);
        assert_eq!(snapshot(&seen), vec![1, 200]);
    }

    #[test]
    fn wide_frontier_matches_reference_across_promotion_move_and_reuse() {
        let ids = (1u128..=40).map(Uuid::from_u128).collect::<Vec<_>>();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        let mut reference = ReferenceQueue::new();

        for (position, &id) in ids.iter().enumerate() {
            let height = u64::try_from(position % 4).unwrap_or(0);
            let payload = u16::try_from(position).unwrap_or(0);
            assert!(
                queue
                    .enqueue(id, height, true, recorded_u16(payload, &seen))
                    .is_none()
            );
            reference.enqueue(id, height, true, payload);
            queue.assert_consistent();
        }
        assert!(queue.frontier.is_large());

        for (position, &id) in ids.iter().take(5).enumerate() {
            let height = u64::try_from(position % 4).unwrap_or(0);
            let event = 100u16.saturating_add(u16::try_from(position).unwrap_or(0));
            assert!(
                queue
                    .enqueue(id, height, false, recorded_u16(event, &seen))
                    .is_none()
            );
            reference.enqueue(id, height, false, event);
            queue.assert_consistent();
            let value = 150u16.saturating_add(u16::try_from(position).unwrap_or(0));
            drop(queue.enqueue(id, height, true, recorded_u16(value, &seen)));
            reference.enqueue(id, height, true, value);
            queue.assert_consistent();
        }

        let moved = ids.first().copied();
        assert!(moved.is_some());
        if let Some(moved) = moved {
            drop(queue.enqueue(moved, 7, true, recorded_u16(250, &seen)));
            reference.enqueue(moved, 7, true, 250);
            queue.assert_consistent();
        }

        while let Some(expected) = reference.pop() {
            let groups = queue.pop_min_height_groups();
            queue.assert_consistent();
            assert!(groups.is_some(), "wide queue drained before reference");
            if let Some(groups) = groups {
                run(groups);
            }
            assert_eq!(drain_u16(&seen), expected);
        }
        assert!(queue.is_empty());
        assert!(!queue.frontier.is_large());

        let id = Uuid::from_u128(100);
        assert!(
            queue
                .enqueue(id, 1, true, recorded_u16(251, &seen))
                .is_none()
        );
        assert!(!queue.frontier.is_large());
        queue.assert_consistent();
        if let Some(groups) = queue.pop_min_height_groups() {
            queue.assert_consistent();
            run(groups);
        }
        assert_eq!(drain_u16(&seen), vec![251]);
    }

    #[test]
    fn matches_original_queue_for_mixed_deterministic_trace() {
        let ids = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut queue = ReadyQueue::new();
        let mut reference = ReferenceQueue::new();

        for step in 0u16..300 {
            let id_index = usize::from(step.checked_rem(3).unwrap_or(0));
            let id = ids.get(id_index);
            assert!(id.is_some(), "generated invalid id index");
            let Some(&id) = id else {
                return;
            };
            let height = u64::from(step.checked_rem(5).unwrap_or(0));
            let coalesce = step.checked_rem(4).unwrap_or(0) != 0;
            let displaced = queue.enqueue(id, height, coalesce, recorded_u16(step, &seen));
            drop(displaced);
            reference.enqueue(id, height, coalesce, step);
            queue.assert_consistent();

            if step.checked_rem(17).unwrap_or(1) == 0 {
                let expected = reference.pop();
                let actual = queue.pop_min_height_groups();
                queue.assert_consistent();
                match (expected, actual) {
                    (Some(expected), Some(actual)) => {
                        run(actual);
                        assert_eq!(drain_u16(&seen), expected);
                    }
                    (None, None) => {}
                    (expected, actual) => {
                        assert_eq!(expected.is_some(), actual.is_some());
                        return;
                    }
                }
            }
        }

        loop {
            let expected = reference.pop();
            let actual = queue.pop_min_height_groups();
            queue.assert_consistent();
            match (expected, actual) {
                (Some(expected), Some(actual)) => {
                    run(actual);
                    assert_eq!(drain_u16(&seen), expected);
                }
                (None, None) => break,
                (expected, actual) => {
                    assert_eq!(expected.is_some(), actual.is_some());
                    break;
                }
            }
        }
    }
}
