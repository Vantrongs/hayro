//! Per-thread measurements of parser and decoder work.

macro_rules! counter {
    ($name:ident, $add:ident, $description:literal) => {
        #[cfg(all(any(debug_assertions, feature = "work-counters"), feature = "std"))]
        std::thread_local! {
            #[doc = $description]
            pub static $name: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
        }
        #[inline(always)]
        pub(crate) fn $add(_count: usize) {
            #[cfg(all(any(debug_assertions, feature = "work-counters"), feature = "std"))]
            $name.set($name.get().saturating_add(_count));
        }
    };
}
counter!(
    BODIES_READ,
    bodies,
    "Bytes examined reading object bodies, including decoded object streams."
);
counter!(
    EXAMINED,
    examined,
    "Individual bytes examined by the byte reader."
);
counter!(
    LEXED,
    lexed,
    "Bytes parsed in dictionary entries and array items."
);
counter!(
    TOLD,
    told,
    "Bytes processed by decoders and cross-reference sections."
);
counter!(
    OBJECT_STREAM_ENTRIES,
    entries,
    "Object stream header entries parsed."
);

#[cfg(all(any(debug_assertions, feature = "work-counters"), feature = "std"))]
std::thread_local! {
    static IN_BODY: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

#[inline]
pub(crate) fn body<T>(read: impl FnOnce() -> T) -> T {
    #[cfg(all(any(debug_assertions, feature = "work-counters"), feature = "std"))]
    if !IN_BODY.replace(true) {
        struct Reading(usize);
        impl Drop for Reading {
            fn drop(&mut self) {
                bodies(EXAMINED.get() - self.0);
                IN_BODY.set(false);
            }
        }
        let _reading = Reading(EXAMINED.get());
        return read();
    }
    read()
}
