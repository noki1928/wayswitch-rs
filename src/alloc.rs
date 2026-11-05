//! Аллокатор, затирающий память при освобождении.
//!
//! Набранный текст неизбежно копируется: строки детектора, очередь событий,
//! буферы D-Bus. Следить за каждой копией ненадёжно, поэтому затирается вся
//! куча Rust. `realloc` по умолчанию идёт через alloc + copy + dealloc, так что
//! старый блок при росте тоже затирается.
//!
//! Что это НЕ покрывает: стек и регистры, ещё живые блоки (например, внутренний
//! буфер событий evdev до следующего чтения), память C-библиотек и ядра.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};

pub struct ZeroingAlloc;

unsafe impl GlobalAlloc for ZeroingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // explicit_bzero компилятор не имеет права выбросить.
        libc::explicit_bzero(ptr.cast(), layout.size());
        System.dealloc(ptr, layout);
    }
}
