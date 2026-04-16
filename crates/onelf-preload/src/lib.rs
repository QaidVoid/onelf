//! LD_PRELOAD library that intercepts execve() for onelf cross-libc portability.
//!
//! When running inside an onelf package (ONELF_PKG_ROOT set) and the target is
//! an ELF binary, this library intercepts execve() and uses userland-execve
//! with the bundled interpreter, ensuring spawned processes use the same libc
//! as the packed binary.

use std::ffi::{CStr, CString};
use std::io::Read;
use std::path::Path;
use std::ptr::addr_of_mut;

type ExecveFn = unsafe extern "C" fn(*const i8, *const *const i8, *const *const i8) -> i32;

static mut REAL_EXECVE: Option<ExecveFn> = None;

fn is_elf(path: &CStr) -> bool {
    let path_str = match path.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let mut file = match std::fs::File::open(path_str) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let mut magic = [0u8; 4];
    if file.read(&mut magic).unwrap_or(0) < 4 {
        return false;
    }
    magic == *b"\x7fELF"
}

fn get_bundled_interp() -> Option<CString> {
    let pkg_root = std::env::var("ONELF_PKG_ROOT").ok()?;
    let interp_rel = std::env::var("ONELF_INTERP_REL").ok()?;

    let interp_path = Path::new(&pkg_root).join(&interp_rel);
    if !interp_path.exists() {
        return None;
    }

    CString::new(interp_path.to_string_lossy().into_owned()).ok()
}

unsafe fn do_userland_execve(
    pathname: *const i8,
    argv: *const *const i8,
    envp: *const *const i8,
    interp: &CString,
) -> ! {
    let exec_path = CStr::from_ptr(pathname);
    let exec_path_std = Path::new(exec_path.to_str().unwrap());

    let interp_path = Path::new(interp.to_str().unwrap());
    let mut options = userland_execve::ExecOptions::new(interp_path);

    let exec_path_c = CString::from(exec_path);
    options.arg(&exec_path_c);

    let mut argp = argv;
    if !argp.is_null() {
        argp = argp.add(1);
        while !(*argp).is_null() {
            let arg = CStr::from_ptr(*argp);
            let arg_c = CString::from(arg);
            options.arg(&arg_c);
            argp = argp.add(1);
        }
    }

    let mut envpp = envp;
    if !envpp.is_null() {
        while !(*envpp).is_null() {
            let env = CStr::from_ptr(*envpp);
            let env_c = CString::from(env);
            if let Some(eq_pos) = env_c.to_bytes().iter().position(|&b| b == b'=') {
                let key = CString::new(&env_c.to_bytes()[..eq_pos]).unwrap();
                let value = CString::new(&env_c.to_bytes()[eq_pos + 1..]).unwrap();
                options.env(&key, &value);
            }
            envpp = envpp.add(1);
        }
    }

    options.override_interpreter(Some(exec_path_std));

    userland_execve::exec_with_options(options)
}

unsafe fn set_errno(err: i32) {
    *libc::__errno_location() = err;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn execve(
    pathname: *const i8,
    argv: *const *const i8,
    envp: *const *const i8,
) -> i32 {
    let real_execve = unsafe {
        if (*addr_of_mut!(REAL_EXECVE)).is_none() {
            let name = CString::new("execve").unwrap();
            let ptr = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
            *addr_of_mut!(REAL_EXECVE) = Some(std::mem::transmute(ptr));
        }
        (*addr_of_mut!(REAL_EXECVE)).unwrap()
    };

    if pathname.is_null() || argv.is_null() || envp.is_null() {
        set_errno(libc::ENOENT);
        return -1;
    }

    let path_cstr = CStr::from_ptr(pathname);

    if is_elf(path_cstr) {
        if let Some(interp) = get_bundled_interp() {
            do_userland_execve(pathname, argv, envp, &interp);
        }
    }

    real_execve(pathname, argv, envp)
}
