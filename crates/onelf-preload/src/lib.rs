//! LD_PRELOAD library that intercepts execve() for onelf cross-libc portability.
//!
//! When a process tries to exec a binary with patched PT_INTERP (starting with
//! /tmp/.oi), this library intercepts the call and uses userland-execve instead
//! of the kernel's execve. This allows spawned processes to work without needing
//! symlinks in /tmp.

use std::ffi::{CStr, CString};
use std::io::Read;
use std::path::Path;
use std::ptr::addr_of_mut;

type ExecveFn = unsafe extern "C" fn(*const i8, *const *const i8, *const *const i8) -> i32;

static mut REAL_EXECVE: Option<ExecveFn> = None;

fn read_elf_interp(data: &[u8]) -> Option<String> {
    if data.len() < 64 || data[0..4] != *b"\x7fELF" {
        return None;
    }

    let class = data[4];

    let (e_phoff, e_phentsize, e_phnum) = match class {
        2 => {
            let e_phoff = u64::from_le_bytes(data[32..40].try_into().ok()?) as usize;
            let e_phentsize = u16::from_le_bytes(data[54..56].try_into().ok()?) as usize;
            let e_phnum = u16::from_le_bytes(data[56..58].try_into().ok()?) as usize;
            (e_phoff, e_phentsize, e_phnum)
        }
        1 => {
            let e_phoff = u32::from_le_bytes(data[28..32].try_into().ok()?) as usize;
            let e_phentsize = u16::from_le_bytes(data[42..44].try_into().ok()?) as usize;
            let e_phnum = u16::from_le_bytes(data[44..46].try_into().ok()?) as usize;
            (e_phoff, e_phentsize, e_phnum)
        }
        _ => return None,
    };

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + e_phentsize > data.len() {
            break;
        }

        let p_type = u32::from_le_bytes(data[off..off + 4].try_into().ok()?);
        if p_type != 3 {
            continue;
        }

        let (p_offset, p_filesz) = match class {
            2 => {
                let p_offset =
                    u64::from_le_bytes(data[off + 8..off + 16].try_into().ok()?) as usize;
                let p_filesz =
                    u64::from_le_bytes(data[off + 32..off + 40].try_into().ok()?) as usize;
                (p_offset, p_filesz)
            }
            1 => {
                let p_offset = u32::from_le_bytes(data[off + 4..off + 8].try_into().ok()?) as usize;
                let p_filesz =
                    u32::from_le_bytes(data[off + 16..off + 20].try_into().ok()?) as usize;
                (p_offset, p_filesz)
            }
            _ => return None,
        };

        if p_offset + p_filesz > data.len() {
            return None;
        }

        let interp = &data[p_offset..p_offset + p_filesz];
        let interp = match interp.iter().position(|&b| b == 0) {
            Some(pos) => &interp[..pos],
            None => interp,
        };
        return std::str::from_utf8(interp).ok().map(String::from);
    }

    None
}

fn has_patched_interp(path: &CStr) -> bool {
    let path_str = match path.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let mut file = match std::fs::File::open(path_str) {
        Ok(f) => f,
        Err(_) => return false,
    };

    let mut buf = vec![0u8; 8192];
    let n = match file.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf.truncate(n);

    match read_elf_interp(&buf) {
        Some(interp) => interp.starts_with("/tmp/.oi"),
        None => false,
    }
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

    if has_patched_interp(path_cstr) {
        if let Some(interp) = get_bundled_interp() {
            do_userland_execve(pathname, argv, envp, &interp);
        }
    }

    real_execve(pathname, argv, envp)
}
