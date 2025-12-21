# Refactoring Notes: Bidirectional Relationship Pattern

## Problem Statement

The dependency tracking system in rx3 requires maintaining **bidirectional relationships** between cells:

1. **Forward edge (parent → child)**: Subscribers - used for reactive propagation
2. **Backward edge (child → parent)**: Dependencies - used for introspection

Originally, this was done implicitly, which was error-prone and hard to maintain.

## The Challenge

When creating a derived cell, we must **always** set up both sides:

```rust
// Creating y = x.map(|v| v * 2)

// Side 1: Track x as y's dependency
let deps = vec![(x.id(), x.name())];
let y = Cell::new_with_dependencies(initial, deps);

// Side 2: Register y's update callback with x
x.watch(move |value| {
    y.set(transform(value));
});
```

**What could go wrong:**
- Forget to track dependencies → introspection breaks
- Forget to register subscriber → reactivity breaks
- Mismatch between the two → inconsistent state

## Solution: Explicit Documentation

Instead of trying to hide the complexity, we make it **explicitly visible** with clear comments.

### In `map()` Method

```rust
// BIDIRECTIONAL RELATIONSHIP SETUP:
// 1. Backward edge (child → parent): Track this cell as a dependency
let parent_info = (self.id, self.name());
let new_cell = Cell::new_with_dependencies(initial, vec![parent_info]);

// 2. Forward edge (parent → child): Register subscriber callback
let c_clone = new_cell.clone();
self.watch(move |value| {
    c_clone.set(transform(value));
});
```

### In `combine!()` Macro

```rust
// BIDIRECTIONAL RELATIONSHIP SETUP:
// 1. Backward edge (child → parents): Track parent cells as dependencies
let mut deps = Vec::new();
$(deps.push(($cell.id(), $cell.name()));)+
let new_cell = Cell::new_with_dependencies(initial, deps);

// 2. Forward edge (parents → child): Register subscriber callbacks (in @watch_all)
combine!(@capture [] [$($cell),+] [$($param),+] compute new_cell)
```

## Why This Approach?

### Considered Alternatives

1. **Helper function** - Attempted but doesn't work well because:
   - Each use case has different callback logic
   - Hard to make generic across `map()` and `combine!()`
   - Would require complex trait bounds

2. **Builder pattern** - Would require:
   - New API surface area
   - More complexity for users
   - Breaking changes to existing code

3. **Automatic tracking** - Impossible because:
   - No way to introspect closure contents
   - Can't know at watch() time which cell the callback belongs to

### Chosen Solution Benefits

✅ **No new abstractions** - Uses existing code structure
✅ **Self-documenting** - Comments make the pattern explicit
✅ **Easy to verify** - Look for "BIDIRECTIONAL RELATIONSHIP SETUP" comment
✅ **No performance cost** - Zero runtime overhead
✅ **Maintainable** - Clear what needs to happen in each location

## Maintenance Checklist

When adding new ways to create derived cells, ensure:

- [ ] Both sides of the relationship are established
- [ ] Comments clearly mark the bidirectional setup
- [ ] Dependencies are collected before creating the cell
- [ ] Subscriber callbacks are registered after creating the cell
- [ ] The pattern matches existing `map()` and `combine!()` implementations

## Code Review Guidelines

When reviewing changes to cell creation, check:

1. **Is dependency metadata collected?**
   ```rust
   let deps = vec![(parent.id(), parent.name())];
   ```

2. **Is the cell created with dependencies?**
   ```rust
   Cell::new_with_dependencies(initial_value, deps)
   ```

3. **Are subscriber callbacks registered?**
   ```rust
   parent.watch(move |value| { /* ... */ });
   ```

4. **Is the pattern documented?**
   ```rust
   // BIDIRECTIONAL RELATIONSHIP SETUP:
   ```

## Future Improvements

Possible enhancements that maintain the explicit pattern:

1. **Validation helper** (internal only)
   ```rust
   #[cfg(debug_assertions)]
   fn validate_bidirectional_setup(cell: &Cell<T, M>) {
       // Check that dependencies match subscriber sources
   }
   ```

2. **Test utilities**
   ```rust
   #[cfg(test)]
   fn assert_dependency_consistency(child: &Cell<U, M>, parents: &[&Cell<T, M>]) {
       assert_eq!(child.dependency_count(), parents.len());
   }
   ```

3. **Documentation tests**
   - Ensure examples show the pattern
   - Verify both sides are always present

## Conclusion

The bidirectional relationship pattern is **intentionally explicit** rather than abstracted away. This makes the code more maintainable and easier to reason about, even though it requires discipline to maintain both sides correctly.

The pattern is:
1. **Collect** parent metadata
2. **Create** cell with dependencies
3. **Register** subscriber callbacks

Always in that order. Always both sides. Always documented.