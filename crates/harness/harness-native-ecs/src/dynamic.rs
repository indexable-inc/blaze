//! Components whose type is known only at run time: a swappable wasm module names one, gives its size,
//! alignment and a layout hash, and reads and writes its value as bytes (docs/hot-reload.md).
//!
//! The data is plain bytes (no hooks: a module's component holds no pointers, see `den-plugin-abi`), kept in
//! the flecs world under a component named by the module, so a reloaded module that registers the same name
//! gets the same handle back and finds its data where it left it. A changed layout is a different component:
//! [`Store::dynamic_relayout`] rebuilds it from values the caller has already converted, in one step.

use std::ffi::CString;

use crate::{Entity, Id, MAX_ALIGN, Store, sys};

/// The most bytes one dynamic component value may hold.
pub const MAX_DYNAMIC_SIZE: usize = 1 << 16;

/// A handle to a dynamic component: an index into the store's table, stable for the life of the store (a
/// relayout keeps it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DynamicId(pub u32);

/// What a dynamic component looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicInfo {
    pub name: String,
    pub size: usize,
    pub align: usize,
    /// A hash of the type's shape, chosen by the module: equal hash means the same layout.
    pub layout: u32,
}

pub(crate) struct DynamicComp {
    info: DynamicInfo,
    id: Id,
}

/// Why a dynamic-component call was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DynamicError {
    /// A name must be a non-empty identifier (letters, digits, underscore).
    BadName(String),
    /// Size or alignment is one flecs storage cannot hold.
    BadLayout {
        size: usize,
        align: usize,
    },
    /// The name is registered with a different layout; use `dynamic_relayout`.
    LayoutChanged {
        old: DynamicInfo,
    },
    UnknownComponent(DynamicId),
    /// A value of the wrong length.
    WrongSize {
        want: usize,
        got: usize,
    },
    NotAlive(Entity),
}

impl std::fmt::Display for DynamicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DynamicError::BadName(n) => write!(f, "{n:?} is not a component name"),
            DynamicError::BadLayout { size, align } => {
                write!(f, "size {size} align {align} is not storable")
            }
            DynamicError::LayoutChanged { old } => {
                write!(f, "{} is registered with another layout", old.name)
            }
            DynamicError::UnknownComponent(id) => write!(f, "no dynamic component {}", id.0),
            DynamicError::WrongSize { want, got } => {
                write!(f, "a value of {got} bytes for a component of {want}")
            }
            DynamicError::NotAlive(e) => write!(f, "{e:?} is not a live entity"),
        }
    }
}

impl std::error::Error for DynamicError {}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn valid_layout(size: usize, align: usize) -> bool {
    size <= MAX_DYNAMIC_SIZE
        && (if size == 0 {
            align == 1
        } else {
            align.is_power_of_two() && align <= MAX_ALIGN && size.is_multiple_of(align)
        })
}

impl Store {
    /// The handle and layout of the dynamic component called `name`, if registered.
    pub fn dynamic_named(&self, name: &str) -> Option<(DynamicId, &DynamicInfo)> {
        self.dynamics
            .iter()
            .position(|d| d.info.name == name)
            .map(|i| (DynamicId(i as u32), &self.dynamics[i].info))
    }

    pub fn dynamic_info(&self, id: DynamicId) -> Option<&DynamicInfo> {
        self.dynamics.get(id.0 as usize).map(|d| &d.info)
    }

    /// Register a component by name, or find it again: the same name with the same layout gives the same
    /// handle. The same name with another layout is `LayoutChanged` and changes nothing.
    pub fn register_dynamic(
        &mut self,
        name: &str,
        size: usize,
        align: usize,
        layout: u32,
    ) -> Result<DynamicId, DynamicError> {
        if !valid_name(name) {
            return Err(DynamicError::BadName(name.to_owned()));
        }
        if !valid_layout(size, align) {
            return Err(DynamicError::BadLayout { size, align });
        }
        if let Some((id, info)) = self.dynamic_named(name) {
            return if info.size == size && info.align == align && info.layout == layout {
                Ok(id)
            } else {
                Err(DynamicError::LayoutChanged { old: info.clone() })
            };
        }
        let id = self.init_dynamic(name, size, align);
        self.dynamics.push(DynamicComp {
            info: DynamicInfo {
                name: name.to_owned(),
                size,
                align,
                layout,
            },
            id,
        });
        Ok(DynamicId(self.dynamics.len() as u32 - 1))
    }

    fn init_dynamic(&mut self, name: &str, size: usize, align: usize) -> Id {
        let cname = CString::new(name).expect("a name checked to be an identifier");
        // SAFETY: zeroed descs are flecs's "unset"; the name is NUL terminated for the call and copied by
        // flecs; a fresh named entity becomes a component of the given size and alignment, with no hooks
        // (plain bytes need none).
        unsafe {
            let mut ed: sys::ecs_entity_desc_t = std::mem::zeroed();
            ed.name = cname.as_ptr();
            let e = sys::ecs_entity_init(self.world, &ed);
            let mut d: sys::ecs_component_desc_t = std::mem::zeroed();
            d.entity = e;
            if size != 0 {
                d.type_.size = size as i32;
                d.type_.alignment = align as i32;
            }
            let id = sys::ecs_component_init(self.world, &d);
            assert!(id != 0, "flecs refused dynamic component {name}");
            id
        }
    }

    /// Set `entity`'s value of `id` (exactly `size` bytes).
    pub fn dynamic_set(
        &mut self,
        entity: Entity,
        id: DynamicId,
        bytes: &[u8],
    ) -> Result<(), DynamicError> {
        let comp = self
            .dynamics
            .get(id.0 as usize)
            .ok_or(DynamicError::UnknownComponent(id))?;
        if bytes.len() != comp.info.size {
            return Err(DynamicError::WrongSize {
                want: comp.info.size,
                got: bytes.len(),
            });
        }
        let cid = comp.id;
        self.check(entity)
            .map_err(|_| DynamicError::NotAlive(entity))?;
        // SAFETY: `entity` is live and writable; `cid` is a component of exactly bytes.len() bytes, and the
        // pointer is valid for that many.
        unsafe {
            if bytes.is_empty() {
                sys::ecs_add_id(self.world, entity.0, cid);
            } else {
                sys::ecs_set_id(
                    self.world,
                    entity.0,
                    cid,
                    bytes.len(),
                    bytes.as_ptr().cast(),
                );
            }
        }
        Ok(())
    }

    /// `entity`'s value of `id`, if it has one (empty for a tag).
    pub fn dynamic_get(&self, entity: Entity, id: DynamicId) -> Option<&[u8]> {
        let comp = self.dynamics.get(id.0 as usize)?;
        if !self.is_alive(entity) {
            return None;
        }
        // SAFETY: the entity is alive; the returned pointer addresses `size` bytes of the column until the
        // store is next written, which `&self` rules out for the borrow's life.
        unsafe {
            if comp.info.size == 0 {
                return sys::ecs_has_id(self.world, entity.0, comp.id).then_some(&[][..]);
            }
            let p = sys::ecs_get_id(self.world, entity.0, comp.id).cast::<u8>();
            (!p.is_null()).then(|| std::slice::from_raw_parts(p, comp.info.size))
        }
    }

    /// Every entity that has `id`.
    pub fn dynamic_each(&self, id: DynamicId) -> Vec<Entity> {
        let Some(comp) = self.dynamics.get(id.0 as usize) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // SAFETY: the iterator is driven to completion; `entities` is valid for `count` ids until the next call.
        unsafe {
            let mut it = sys::ecs_each_id(self.world, comp.id);
            while sys::ecs_each_next(&mut it) {
                out.extend_from_slice(std::slice::from_raw_parts(
                    it.entities.cast::<Entity>(),
                    it.count as usize,
                ));
            }
        }
        out
    }

    /// Give `id` a new layout and the values the caller converted for it: every entity that had the
    /// component loses the old one and gets its entry in `values` (an entity not in `values` loses it).
    /// Checked first, then applied; the handle stays, the flecs component is a new one.
    pub fn dynamic_relayout(
        &mut self,
        id: DynamicId,
        size: usize,
        align: usize,
        layout: u32,
        values: &[(Entity, Vec<u8>)],
    ) -> Result<(), DynamicError> {
        let old = self
            .dynamics
            .get(id.0 as usize)
            .ok_or(DynamicError::UnknownComponent(id))?;
        if !valid_layout(size, align) {
            return Err(DynamicError::BadLayout { size, align });
        }
        for (e, v) in values {
            if v.len() != size {
                return Err(DynamicError::WrongSize {
                    want: size,
                    got: v.len(),
                });
            }
            if !self.is_writable(*e) {
                return Err(DynamicError::NotAlive(*e));
            }
        }
        let (name, old_id) = (old.info.name.clone(), old.id);
        // SAFETY: `old_id` is a component entity this store made; deleting it removes it from its
        // entities (flecs's default cleanup for a deleted component).
        unsafe { sys::ecs_delete(self.world, old_id) };
        let new_id = self.init_dynamic(&name, size, align);
        self.dynamics[id.0 as usize] = DynamicComp {
            info: DynamicInfo {
                name,
                size,
                align,
                layout,
            },
            id: new_id,
        };
        for (e, v) in values {
            self.dynamic_set(*e, id, v)?;
        }
        Ok(())
    }
}
