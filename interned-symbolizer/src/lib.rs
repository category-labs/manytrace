use blazesym::symbolize::source::{Kernel, Process, Source};
use blazesym::symbolize::{Input, Symbolized, Symbolizer as BlazeSymbolizer};
use blazesym::Pid;
use protocol::{Callstack, Frame, InternedData, InternedString, Mapping};
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use thiserror::Error;

const FAKE_MAPPING_ID: u64 = 1;

#[derive(Error, Debug)]
pub enum SymbolizerError {
    #[error("blazesym error: {0}")]
    Blazesym(#[from] blazesym::Error),
    #[error("empty symbols - both user and kernel addresses are empty")]
    EmptySymbols,
}

pub struct InternedCache {
    next_callstack_sequence: u64,
    callstack_hash_to_sequence: HashMap<u64, u64>,

    next_frame_sequence: u64,
    pid_frame_to_sequence: HashMap<(i32, u64), u64>,

    unknown_function_id: Option<u64>,
    next_function_id: u64,
    fake_mapping_id: Option<u64>,
    next_mapping_id: u64,

    is_first_interned_data: bool,
}

impl InternedCache {
    pub fn new() -> Self {
        Self {
            next_callstack_sequence: 0,
            callstack_hash_to_sequence: HashMap::new(),
            next_frame_sequence: 0,
            pid_frame_to_sequence: HashMap::new(),
            unknown_function_id: None,
            next_function_id: 0,
            fake_mapping_id: None,
            next_mapping_id: 0,
            is_first_interned_data: true,
        }
    }

    pub fn cached_frame(&self, pid: i32, frame: u64) -> Option<u64> {
        self.pid_frame_to_sequence.get(&(pid, frame)).copied()
    }

    pub fn cached_callstack(&self, _pid: i32, callstack: &[u64]) -> Option<u64> {
        let hash = Self::hash_callstack(callstack);
        self.callstack_hash_to_sequence.get(&hash).copied()
    }

    pub fn reserve_frame(&mut self, pid: i32, frame: u64) -> u64 {
        *self
            .pid_frame_to_sequence
            .entry((pid, frame))
            .or_insert_with(|| {
                let id = self.next_frame_sequence;
                self.next_frame_sequence += 1;
                id
            })
    }

    pub fn reserve_callstack(&mut self, callstack: Vec<u64>) -> u64 {
        let hash = Self::hash_callstack(&callstack);
        *self
            .callstack_hash_to_sequence
            .entry(hash)
            .or_insert_with(|| {
                let id = self.next_callstack_sequence;
                self.next_callstack_sequence += 1;
                id
            })
    }

    fn hash_callstack(callstack: &[u64]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        callstack.hash(&mut hasher);
        hasher.finish()
    }

    pub fn get_or_create_unknown_function_id(&mut self) -> u64 {
        if let Some(id) = self.unknown_function_id {
            id
        } else {
            let id = self.next_function_id;
            self.next_function_id += 1;
            self.unknown_function_id = Some(id);
            id
        }
    }

    pub fn get_or_create_fake_mapping_id(&mut self) -> u64 {
        if let Some(id) = self.fake_mapping_id {
            id
        } else {
            self.fake_mapping_id = Some(FAKE_MAPPING_ID);
            self.next_mapping_id = FAKE_MAPPING_ID + 1;
            FAKE_MAPPING_ID
        }
    }

    pub fn reserve_function_id(&mut self) -> u64 {
        let id = self.next_function_id;
        self.next_function_id += 1;
        id
    }

    pub fn take_is_first_interned_data(&mut self) -> bool {
        let was_first = self.is_first_interned_data;
        self.is_first_interned_data = false;
        was_first
    }
}

impl Default for InternedCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessConfig {
    pub debug_syms: bool,
    pub perf_map: bool,
    pub map_files: bool,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            debug_syms: true,
            perf_map: false,
            map_files: false,
        }
    }
}

pub struct Symbolizer {
    cache: InternedCache,
    cfg: ProcessConfig,
}

impl Symbolizer {
    pub fn new(cfg: ProcessConfig) -> Self {
        Self {
            cache: InternedCache::new(),
            cfg,
        }
    }

    pub fn session<'a>(&'a mut self, symbolizer: &'a BlazeSymbolizer) -> Session<'a> {
        Session {
            symbolizer,
            cache: &mut self.cache,
            process_config: self.cfg,
        }
    }
}

pub struct Interned<'a> {
    pid: i32,
    user_addresses: &'a [u64],
    kernel_addresses: &'a [u64],
    user_symbols: Vec<Symbolized<'a>>,
    kernel_symbols: Vec<Symbolized<'a>>,
    cache: &'a mut InternedCache,
}

impl<'a> Interned<'a> {
    pub fn data(&'a mut self) -> (u64, Option<InternedData<'a>>) {
        let mut all_frame_ids = Vec::new();
        let mut new_function_names = Vec::new();
        let mut new_frames = Vec::new();
        let mut new_mappings = Vec::new();
        let mut new_build_ids = Vec::new();

        let need_unknown_function = self.cache.unknown_function_id.is_none();
        let need_fake_mapping = self.cache.fake_mapping_id.is_none();

        if need_unknown_function {
            let unknown_id = self.cache.get_or_create_unknown_function_id();
            new_function_names.push(InternedString {
                iid: unknown_id,
                str: Cow::Borrowed("<unknown>"),
            });
        }

        if need_fake_mapping {
            let mapping_id = self.cache.get_or_create_fake_mapping_id();
            new_mappings.push(Mapping {
                iid: mapping_id,
                build_id: 1,
                exact_offset: 0,
                start_offset: 0,
                start: 0,
                end: 0x7fffffffffffffff,
                load_bias: 0,
                path_string_ids: vec![],
            });
            new_build_ids.push(InternedString {
                iid: 1,
                str: Cow::Borrowed("unknown"),
            });
        }

        for (i, sym) in self.user_symbols.iter().enumerate() {
            let addr = self.user_addresses[i];
            let is_new_frame = self.cache.cached_frame(self.pid, addr).is_none();
            let frame_id = self.cache.reserve_frame(self.pid, addr);
            all_frame_ids.push(frame_id);

            if is_new_frame {
                let func_id = if let Some(sym) = sym.as_sym() {
                    let id = self.cache.reserve_function_id();
                    new_function_names.push(InternedString {
                        iid: id,
                        str: Cow::Borrowed(sym.name.as_ref()),
                    });
                    id
                } else {
                    self.cache.get_or_create_unknown_function_id()
                };

                new_frames.push(Frame {
                    iid: frame_id,
                    function_name_id: func_id,
                    mapping_id: FAKE_MAPPING_ID,
                    rel_pc: addr,
                });
            }
        }

        for (i, sym) in self.kernel_symbols.iter().enumerate() {
            let addr = self.kernel_addresses[i];
            let is_new_frame = self.cache.cached_frame(self.pid, addr).is_none();
            let frame_id = self.cache.reserve_frame(self.pid, addr);
            all_frame_ids.push(frame_id);

            if is_new_frame {
                let func_id = if let Some(sym) = sym.as_sym() {
                    let id = self.cache.reserve_function_id();
                    new_function_names.push(InternedString {
                        iid: id,
                        str: Cow::Borrowed(sym.name.as_ref()),
                    });
                    id
                } else {
                    self.cache.get_or_create_unknown_function_id()
                };

                new_frames.push(Frame {
                    iid: frame_id,
                    function_name_id: func_id,
                    mapping_id: FAKE_MAPPING_ID,
                    rel_pc: addr,
                });
            }
        }

        let callstack_id = self.cache.reserve_callstack(all_frame_ids.clone());

        let data =
            if !new_function_names.is_empty() || !new_frames.is_empty() || !new_mappings.is_empty()
            {
                let continuation = !self.cache.take_is_first_interned_data();
                let interned_data = InternedData {
                    function_names: new_function_names,
                    frames: new_frames,
                    callstacks: vec![Callstack {
                        iid: callstack_id,
                        frame_ids: all_frame_ids.clone(),
                    }],
                    mappings: new_mappings,
                    build_ids: new_build_ids,
                    continuation,
                };
                Some(interned_data)
            } else {
                None
            };
        (callstack_id, data)
    }
}

pub struct Session<'a> {
    symbolizer: &'a BlazeSymbolizer,
    cache: &'a mut InternedCache,
    process_config: ProcessConfig,
}

impl<'a> Session<'a> {
    pub fn symbolize(
        &'a mut self,
        pid: i32,
        user_addresses: &'a [u64],
        kernel_addresses: &'a [u64],
    ) -> Result<Interned<'a>, SymbolizerError> {
        if user_addresses.is_empty() && kernel_addresses.is_empty() {
            return Err(SymbolizerError::EmptySymbols);
        }

        let user_symbols = if !user_addresses.is_empty() {
            let source = Source::Process(Process {
                pid: Pid::from(pid as u32),
                debug_syms: self.process_config.debug_syms,
                perf_map: self.process_config.perf_map,
                map_files: self.process_config.map_files,
                _non_exhaustive: (),
            });

            self.symbolizer
                .symbolize(&source, Input::AbsAddr(user_addresses))?
        } else {
            vec![]
        };

        let kernel_symbols = if !kernel_addresses.is_empty() {
            let source = Source::Kernel(Kernel::default());
            self.symbolizer
                .symbolize(&source, Input::AbsAddr(kernel_addresses))?
        } else {
            vec![]
        };

        Ok(Interned {
            pid,
            user_addresses,
            kernel_addresses,
            user_symbols,
            kernel_symbols,
            cache: &mut self.cache,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_caching() {
        let mut cache = InternedCache::new();

        assert_eq!(cache.cached_frame(1234, 0x1000), None);

        let id1 = cache.reserve_frame(1234, 0x1000);
        assert_eq!(id1, 0);

        assert_eq!(cache.cached_frame(1234, 0x1000), Some(0));

        let id2 = cache.reserve_frame(1234, 0x1000);
        assert_eq!(id2, 0);

        let id3 = cache.reserve_frame(1234, 0x2000);
        assert_eq!(id3, 1);

        let id4 = cache.reserve_frame(5678, 0x1000);
        assert_eq!(id4, 2);
    }

    #[test]
    fn test_callstack_caching() {
        let mut cache = InternedCache::new();

        let callstack1 = vec![1, 2, 3];
        assert_eq!(cache.cached_callstack(1234, &callstack1), None);

        let id1 = cache.reserve_callstack(callstack1.clone());
        assert_eq!(id1, 0);

        assert_eq!(cache.cached_callstack(1234, &callstack1), Some(0));

        let id2 = cache.reserve_callstack(callstack1.clone());
        assert_eq!(id2, 0);

        let callstack2 = vec![4, 5, 6];
        let id3 = cache.reserve_callstack(callstack2.clone());
        assert_eq!(id3, 1);

        assert_eq!(cache.cached_callstack(5678, &callstack2), Some(1));
    }

    #[test]
    fn test_callstack_hash_collision() {
        let mut cache = InternedCache::new();

        let callstack1 = vec![1, 2, 3];
        let callstack2 = vec![1, 2, 3];
        let callstack3 = vec![3, 2, 1];

        let id1 = cache.reserve_callstack(callstack1.clone());
        let id2 = cache.reserve_callstack(callstack2.clone());
        let id3 = cache.reserve_callstack(callstack3.clone());

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }
}
