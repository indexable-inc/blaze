# Hyperion patches to vendored flecs (v4.1.6)

1. `flecs_reparent_name_index` (flecs.c, marked `HYPERION PATCH`): upstream `ecs_assert(src_index != dst_index)`; when both name indexes are the same (including both NULL) re-indexing is a no-op, so it logs once at error level (entity path, source and destination table) and returns. Found by the r22 dry boot; the log line names the entity so the real cause can be fixed.

## den patches

2. Upstream 059519d56 ("[script] Fix issue where prefabs defined in scripts aren't cleared on script update", flecs master
   2026-07-15), the `distr/flecs.c` hunks applied as they are: `ecs_script_clear` deletes with `flecs_delete_with(.., true)`, which
   also deletes the script's prefab tables (`ecs_delete_with` leaves prefabs since 4.1.x). Without it `ecs_script_update` of a
   script that defines prefabs trips `assert(!ecs_iter_is_true(&it))` in debug builds and keeps the old prefabs in release; den's
   scene hot reload (den-native) updates such scripts.

3. Soft errors (den, flecs.c, marked `DEN PATCH`): built with `FLECS_SOFT_ASSERT` (hyperion-flecs-c/build.rs), `ecs_check` and `ecs_throw` log and `goto error` instead of aborting. `ecs_add_id` and `ecs_set_id` additionally refuse a pair that breaks a relationship constraint (OneOf target not a child, Relationship used as target, Target used as relationship, IsA a Final) before adding it: upstream only logs that, in FLECS_DEBUG builds, once, when the pair's component record is made, and adds the pair anyway. The refusal is reported by `ecs_log_last_error()` and den-ecs's log hook. See docs/upstream/flecs-soft-errors/.

4. Ordered parts (den, flecs.c bootstrap, marked `DEN PATCH (ordered parts)`): `OrderedChildren` gets `(OnInstantiate, Override)`.
   Upstream leaves it inherited, so an instance of a prefab whose children are ordered does not own the tag when flecs
   instantiates those children, and they come out table by table, not in declaration order. It cannot be set after
   bootstrap: the core's own observer on `OrderedChildren` locks its traits (`flecs_component_is_trait_locked`). den's
   composite prefabs (docs/ui-model.md) depend on it.
