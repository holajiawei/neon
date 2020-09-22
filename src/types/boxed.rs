use std::any::{self, Any};
use std::ops::Deref;

use neon_runtime::raw;
use neon_runtime::external;

use crate::context::{Context, FinalizeContext};
use crate::context::internal::Env;
use crate::handle::{Managed, Handle};
use crate::types::internal::ValueInternal;
use crate::types::Value;

type BoxAny = Box<dyn Any + Send + 'static>;

/// A smart pointer for Rust data managed by the JavaScript engine.
///
/// The type `JsBox<T>` provides shared ownership of a value of type `T`,
/// allocated in the heap. The data is owned by the JavaScript engine and the
/// lifetime is managed by the JavaScript garbage collector.
///
/// Shared references in Rust disallow mutation by default, and `JsBox` is no
/// exception: you cannot generally obtain a mutable reference to something
/// inside a `JsBox`. If you need to mutate through a `JsBox`, use
/// [`Cell`](https://doc.rust-lang.org/std/cell/struct.Cell.html),
/// [`RefCell`](https://doc.rust-lang.org/stable/std/cell/struct.RefCell.html),
/// or one of the other types that provide interior mutability.
///
/// ## `Deref` behavior
///
/// `JsBox<T>` automatically dereferences to `T` (via the `Deref` trait), so
/// you can call `T`'s method on a value of type `JsBox<T>`.
///
/// ```rust
/// # use neon::prelude::*;
/// # fn my_neon_function(mut cx: FunctionContext) -> JsResult<JsUndefined> {
/// let vec: Handle<JsBox<Vec<_>>> = cx.boxed(vec![1, 2, 3]);
///
/// println!("Length: {}", vec.len());
/// # Ok(cx.undefined())
/// # }
/// ```
///
/// ## Examples
///
/// Passing some immutable data between Rust and JavaScript.
///
/// ```rust
/// # use neon::prelude::*;
/// # use std::path::{Path, PathBuf};
/// fn create_path(mut cx: FunctionContext) -> JsResult<JsBox<PathBuf>> {
///     let path = cx.argument::<JsString>(0)?.value(&mut cx);
///     let path = Path::new(&path).to_path_buf();
///
///     Ok(cx.boxed(path))
/// }
///
/// fn print_path(mut cx: FunctionContext) -> JsResult<JsUndefined> {
///     let path = cx.argument::<JsBox<PathBuf>>(0)?;
///
///     println!("{}", path.display());
///
///     Ok(cx.undefined())
/// }
/// ```
///
/// Passing a user defined struct wrapped in a `RefCell` for mutability. This
/// pattern is useful for creating classes in JavaScript.
///
/// ```rust
/// # use neon::prelude::*;
/// # use std::cell::RefCell;
///
/// type BoxedPerson = JsBox<RefCell<Person>>;
///
/// struct Person {
///      name: String,
/// }
///
/// impl Finalize for Person {}
///
/// impl Person {
///     pub fn new(name: String) -> Self {
///         Person { name }
///     }
/// 
///     pub fn set_name(&mut self, name: String) {
///         self.name = name;
///     }
/// 
///     pub fn greet(&self) -> String {
///         format!("Hello, {}!", self.name)
///     }
/// }
/// 
/// fn person_new(mut cx: FunctionContext) -> JsResult<BoxedPerson> {
///     let name = cx.argument::<JsString>(0)?.value(&mut cx);
///     let person = RefCell::new(Person::new(name));
/// 
///     Ok(cx.boxed(person))
/// }
/// 
/// fn person_set_name(mut cx: FunctionContext) -> JsResult<JsUndefined> {
///     let person = cx.argument::<BoxedPerson>(0)?;
///     let mut person = person.borrow_mut();
///     let name = cx.argument::<JsString>(1)?.value(&mut cx);
/// 
///     person.set_name(name);
/// 
///     Ok(cx.undefined())
/// }
/// 
/// fn person_greet(mut cx: FunctionContext) -> JsResult<JsString> {
///     let person = cx.argument::<BoxedPerson>(0)?;
///     let person = person.borrow();
///     let greeting = person.greet();
/// 
///     Ok(cx.string(greeting))
/// }
pub struct JsBox<T: Send + 'static> {
    local: raw::Local,
    // `JsBox` cannot verify the lifetime. Store a raw pointer to force uses
    // to be marked unsafe. In practice, it can be treated as `'static` but
    // should only be exposed as part of a `Handle` tied to a `Context` lifetime.
    internal: *const T,
}

// Custom `Clone` implementation since `T` might not be `Clone`
impl<T: Send + 'static> Clone for JsBox<T> {
    fn clone(&self) -> Self {
        JsBox {
            local: self.local,
            internal: self.internal,
        }
    }
}

impl<T: Send + 'static> Copy for JsBox<T> {}

impl<T: Send + 'static> Value for JsBox<T> { }

impl<T: Send + 'static> Managed for JsBox<T> {
    fn to_raw(self) -> raw::Local {
        self.local
    }

    fn from_raw(env: Env, h: raw::Local) -> Self {
        let v = unsafe {
            external::deref::<BoxAny>(env.to_raw(), h)
                .map(|v| &*v)
        };

        let internal = v
            .and_then(|v| v.downcast_ref())
            .expect("Expected type to already be validated");

        Self {
            local: h,
            internal,
        }        
    }
}

impl<T: Send + 'static> ValueInternal for JsBox<T> {
    fn name() -> String {
        any::type_name::<Self>().to_string()
    }

    fn is_typeof<Other: Value>(env: Env, other: Other) -> bool {
        let v = unsafe {
            external::deref::<BoxAny>(env.to_raw(), other.to_raw())
                .map(|v| &*v)
        };

        v.map(|v| v.is::<T>()).unwrap_or(false)
    }

    fn downcast<Other: Value>(env: Env, other: Other) -> Option<Self> {
        let local = other.to_raw();
        let v = unsafe {
            external::deref::<BoxAny>(env.to_raw(), local)
                .map(|v| &*v)
        };

        v.and_then(|v| v.downcast_ref())
            .map(|internal| Self {
                local,
                internal,
            })
    }
}

impl<T: Finalize + Send + 'static> JsBox<T> {
    /// Constructs a new `JsBox` containing `value`.
    pub fn new<'a, C>(cx: &mut C, value: T) -> Handle<'a, JsBox<T>>
    where
        C: Context<'a>,
        T: Send + 'static,
    {
        fn finalizer<U: Finalize + 'static>(env: raw::Env, data: BoxAny) {
            let data = *data.downcast::<U>().unwrap();
            let env = unsafe { std::mem::transmute(env) };

            FinalizeContext::with(
                env,
                move |mut cx| data.finalize(&mut cx),
            );
        }

        let v = Box::new(value) as BoxAny;
        // Since this value was just constructed, we know it is `T`
        let internal = &*v as *const dyn Any as *const T;
        let local = unsafe {
            external::create(cx.env().to_raw(), v, finalizer::<T>)
        };

        Handle::new_internal(Self {
            local,
            internal,
        })
    }
}

impl<'a, T: Send + 'static> Deref for JsBox<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.internal }
    }
}

/// Finalize is executed on the main JavaScript thread and executed immediately
/// before garbage collection.
pub trait Finalize: Sized {
    fn finalize<'a, C: Context<'a>>(self, _: &mut C) {}
}

// Primitives

impl Finalize for bool {}
impl Finalize for char {}
impl Finalize for i8 {}
impl Finalize for i16 {}
impl Finalize for i32 {}
impl Finalize for i64 {}
impl Finalize for isize {}
impl Finalize for u8 {}
impl Finalize for u16 {}
impl Finalize for u32 {}
impl Finalize for u64 {}
impl Finalize for usize {}
impl Finalize for f32 {}
impl Finalize for f64 {}

// Common types

impl Finalize for String {}
impl Finalize for std::path::PathBuf {}

// Tuples

macro_rules! finalize_tuple_impls {
    ($( $name:ident )+) => {
        impl<$($name: Finalize),+> Finalize for ($($name,)+) {
            fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
                #![allow(non_snake_case)]
                let ($($name,)+) = self;
                ($($name.finalize(cx),)+);
            }
        }
    };
}

impl Finalize for () {}
finalize_tuple_impls! { T0 }
finalize_tuple_impls! { T0 T1 }
finalize_tuple_impls! { T0 T1 T2 }
finalize_tuple_impls! { T0 T1 T2 T3 }
finalize_tuple_impls! { T0 T1 T2 T3 T4 }
finalize_tuple_impls! { T0 T1 T2 T3 T4 T5 }
finalize_tuple_impls! { T0 T1 T2 T3 T4 T5 T6 }
finalize_tuple_impls! { T0 T1 T2 T3 T4 T5 T6 T7 }

// Collections

impl<T: Finalize> Finalize for Vec<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        for item in self {
            item.finalize(cx);
        }
    }
}

// Smart Pointers

impl<T: Finalize> Finalize for std::boxed::Box<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        (*self).finalize(cx);
    }
}

impl<T: Finalize> Finalize for std::rc::Rc<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        if let Ok(v) = std::rc::Rc::try_unwrap(self) {
            v.finalize(cx);
        }
    }
}

impl<T: Finalize> Finalize for std::sync::Arc<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        if let Ok(v) = std::sync::Arc::try_unwrap(self) {
            v.finalize(cx);
        }
    }
}

impl<T: Finalize> Finalize for std::sync::Mutex<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        if let Ok(v) = self.into_inner() {
            v.finalize(cx);
        }
    }
}

impl<T: Finalize> Finalize for std::sync::RwLock<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        if let Ok(v) = self.into_inner() {
            v.finalize(cx);
        }
    }
}

impl<T: Finalize> Finalize for std::cell::Cell<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        self.into_inner().finalize(cx);
    }
}

impl<T: Finalize> Finalize for std::cell::RefCell<T> {
    fn finalize<'a, C: Context<'a>>(self, cx: &mut C) {
        self.into_inner().finalize(cx);
    }
}
