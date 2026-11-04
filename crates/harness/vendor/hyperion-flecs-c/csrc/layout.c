/* The sizes of the structs Rust reads or writes through the bindings, for a test that compares
 * them with the Rust side on every target. */
#include <stddef.h>
#include "flecs.h"

size_t den_flecs_layout(int which) {
    switch (which) {
    case 0: return sizeof(ecs_os_api_t);
    case 1: return sizeof(ecs_type_hooks_t);
    case 2: return sizeof(ecs_type_info_t);
    case 3: return sizeof(ecs_component_desc_t);
    case 4: return sizeof(ecs_entity_desc_t);
    case 5: return sizeof(ecs_entities_t);
    case 6: return sizeof(ecs_iter_t);
    case 7: return sizeof(ecs_time_t);
    default: return 0;
    }
}
