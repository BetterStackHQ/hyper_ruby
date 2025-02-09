use std::{ffi::c_void, mem::MaybeUninit, ptr::null_mut};
use rb_sys::rb_thread_call_without_gvl;

// Call a specified function, having released the GVL. Acquires the GVL before returning.
pub(crate) fn nogvl<F, R>(mut func: F) -> R
where
    F: FnMut() -> R,
    R: Sized,
{
    unsafe extern "C" fn call_without_gvl<F, R>(arg: *mut c_void) -> *mut c_void
    where
        F: FnMut() -> R,
        R: Sized,
    {
        let arg = arg as *mut (&mut F, &mut MaybeUninit<R>);
        let (func, result) = unsafe { &mut *arg };
        result.write(func());
        null_mut()
    }
    let result = MaybeUninit::uninit();
    let arg_ptr = &(&mut func, &result) as *const _ as *mut c_void;
    unsafe {
        rb_thread_call_without_gvl(Some(call_without_gvl::<F, R>), arg_ptr, None, null_mut());
        result.assume_init()
    }
} 