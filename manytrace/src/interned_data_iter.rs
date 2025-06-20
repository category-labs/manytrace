use protocol::{ArchivedInternedData, InternedData};
use rkyv::Archived;

pub trait CallstackIterable {
    type Item;
    fn iid(&self) -> u64;
    fn frame_ids(&self) -> Vec<u64>;
}

pub trait FrameIterable {
    fn iid(&self) -> u64;
    fn function_name_id(&self) -> u64;
    fn rel_pc(&self) -> u64;
}

pub trait InternedStringIterable {
    fn iid(&self) -> u64;
    fn str_ref(&self) -> &str;
}

pub trait InternedDataIterable<'a> {
    type Callstack: CallstackIterable + 'a;
    type Frame: FrameIterable + 'a;
    type FunctionName: InternedStringIterable + 'a;

    type CallstackIter: Iterator<Item = &'a Self::Callstack> + Clone;
    type FrameIter: Iterator<Item = &'a Self::Frame> + Clone;
    type FunctionNameIter: Iterator<Item = &'a Self::FunctionName> + Clone;

    fn callstacks(&'a self) -> Self::CallstackIter;
    fn frames(&'a self) -> Self::FrameIter;
    fn function_names(&'a self) -> Self::FunctionNameIter;
}

impl CallstackIterable for protocol::Callstack {
    type Item = u64;

    fn iid(&self) -> u64 {
        self.iid
    }

    fn frame_ids(&self) -> Vec<u64> {
        self.frame_ids.clone()
    }
}

impl FrameIterable for protocol::Frame {
    fn iid(&self) -> u64 {
        self.iid
    }

    fn function_name_id(&self) -> u64 {
        self.function_name_id
    }

    fn rel_pc(&self) -> u64 {
        self.rel_pc
    }
}

impl<'a> InternedStringIterable for protocol::InternedString<'a> {
    fn iid(&self) -> u64 {
        self.iid
    }

    fn str_ref(&self) -> &str {
        self.str.as_ref()
    }
}

impl<'a> InternedDataIterable<'a> for InternedData<'a> {
    type Callstack = protocol::Callstack;
    type Frame = protocol::Frame;
    type FunctionName = protocol::InternedString<'a>;

    type CallstackIter = std::slice::Iter<'a, protocol::Callstack>;
    type FrameIter = std::slice::Iter<'a, protocol::Frame>;
    type FunctionNameIter = std::slice::Iter<'a, protocol::InternedString<'a>>;

    fn callstacks(&'a self) -> Self::CallstackIter {
        self.callstacks.iter()
    }

    fn frames(&'a self) -> Self::FrameIter {
        self.frames.iter()
    }

    fn function_names(&'a self) -> Self::FunctionNameIter {
        self.function_names.iter()
    }
}

impl CallstackIterable for protocol::ArchivedCallstack {
    type Item = Archived<u64>;

    fn iid(&self) -> u64 {
        self.iid.to_native()
    }

    fn frame_ids(&self) -> Vec<u64> {
        self.frame_ids.iter().map(|id| id.to_native()).collect()
    }
}

impl FrameIterable for protocol::ArchivedFrame {
    fn iid(&self) -> u64 {
        self.iid.to_native()
    }

    fn function_name_id(&self) -> u64 {
        self.function_name_id.to_native()
    }

    fn rel_pc(&self) -> u64 {
        self.rel_pc.to_native()
    }
}

impl<'a> InternedStringIterable for protocol::ArchivedInternedString<'a> {
    fn iid(&self) -> u64 {
        self.iid.to_native()
    }

    fn str_ref(&self) -> &str {
        self.str.as_ref()
    }
}

type ArchivedVecIter<'a, T> = std::slice::Iter<'a, T>;

impl<'a> InternedDataIterable<'a> for ArchivedInternedData<'a> {
    type Callstack = protocol::ArchivedCallstack;
    type Frame = protocol::ArchivedFrame;
    type FunctionName = protocol::ArchivedInternedString<'a>;

    type CallstackIter = ArchivedVecIter<'a, protocol::ArchivedCallstack>;
    type FrameIter = ArchivedVecIter<'a, protocol::ArchivedFrame>;
    type FunctionNameIter = ArchivedVecIter<'a, protocol::ArchivedInternedString<'a>>;

    fn callstacks(&'a self) -> Self::CallstackIter {
        self.callstacks.as_slice().iter()
    }

    fn frames(&'a self) -> Self::FrameIter {
        self.frames.as_slice().iter()
    }

    fn function_names(&'a self) -> Self::FunctionNameIter {
        self.function_names.as_slice().iter()
    }
}
