//! One-shot Rust adapter to the device's existing AArch64 Keymaster library.
//! It never provisions, imports, deletes, upgrades or attests keys. Private-key
//! export is deliberately absent. The parent owns storage, policy and auth.
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    ptr,
};

const MAX_BLOB: usize = 8192;
const MAX_MESSAGE: usize = 512;
const BAD_REQUEST: i32 = -1001;
const UNAVAILABLE: i32 = -1002;
const INVALID_REPLY: i32 = -1003;
const WRONG_KEY: i32 = -1004;

#[repr(C)]
#[derive(Clone, Copy)]
struct Blob {
    data: *mut u8,
    size: usize,
}
impl Blob {
    fn empty() -> Self {
        Self {
            data: ptr::null_mut(),
            size: 0,
        }
    }
    fn from(bytes: &[u8]) -> Self {
        Self {
            data: bytes.as_ptr().cast_mut(),
            size: bytes.len(),
        }
    }
    fn copy(&self, limit: usize) -> Result<Vec<u8>, i32> {
        if self.data.is_null() || self.size == 0 || self.size > limit {
            return Err(INVALID_REPLY);
        }
        // Only fixed vendor output structures are accepted, with bounded sizes.
        Ok(unsafe { std::slice::from_raw_parts(self.data, self.size) }.to_vec())
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
union ParamValue {
    num: u32,
    wide: u64,
    blob: Blob,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Param {
    tag: u32,
    value: ParamValue,
}
impl Param {
    fn number(tag: u32, value: u32) -> Self {
        // Keep the unused union storage deterministic across the native ABI.
        let mut parameter: Self = unsafe { std::mem::zeroed() };
        parameter.tag = tag;
        parameter.value.num = value;
        parameter
    }
}
#[repr(C)]
struct Params {
    values: *mut Param,
    count: usize,
}
impl Params {
    fn empty() -> Self {
        Self {
            values: ptr::null_mut(),
            count: 0,
        }
    }
    fn from(values: &mut [Param]) -> Self {
        Self {
            values: values.as_mut_ptr(),
            count: values.len(),
        }
    }
}
#[repr(C)]
struct Characteristics {
    hardware: Params,
    software: Params,
}
const _: () = assert!(std::mem::size_of::<Param>() == 24);
const _: () = assert!(std::mem::offset_of!(Param, value) == 8);

type Init = unsafe extern "C" fn(*const Blob, u32, u32) -> i32;
type Close = unsafe extern "C" fn() -> i32;
type Generate = unsafe extern "C" fn(*const Params, *mut Blob, *mut Characteristics) -> i32;
type CharacteristicsFn =
    unsafe extern "C" fn(*const Blob, *const Blob, *const Blob, *mut Characteristics) -> i32;
type Export = unsafe extern "C" fn(i32, *const Blob, *const Blob, *const Blob, *mut Blob) -> i32;
type Begin = unsafe extern "C" fn(i32, *const Blob, *const Params, *mut Params, *mut u64) -> i32;
type Finish = unsafe extern "C" fn(
    u64,
    *const Params,
    *const Blob,
    *const Blob,
    *mut Params,
    *mut Blob,
) -> i32;
type Abort = unsafe extern "C" fn(u64) -> i32;

unsafe extern "C" {
    fn dlopen(name: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dup(fd: c_int) -> c_int;
    fn dup2(old: c_int, new: c_int) -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    fn alarm(seconds: u32) -> u32;
    fn setrlimit(resource: c_int, limits: *const [u64; 2]) -> c_int;
}

struct Keymaster {
    close: Close,
    generate: Generate,
    characteristics: CharacteristicsFn,
    export: Export,
    begin: Begin,
    finish: Finish,
    abort: Abort,
}
fn status(code: i32) -> Result<(), i32> {
    if code == 0 { Ok(()) } else { Err(code) }
}
unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> Result<T, i32> {
    let address = unsafe { dlsym(handle, name.as_ptr()) };
    if address.is_null() || std::mem::size_of::<T>() != std::mem::size_of_val(&address) {
        return Err(UNAVAILABLE);
    }
    Ok(unsafe { std::mem::transmute_copy(&address) })
}
impl Keymaster {
    fn open() -> Result<Self, i32> {
        if !cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            return Err(UNAVAILABLE);
        }
        let library = unsafe { dlopen(c"/usr/lib/libKeyMaster.so.0.0.0".as_ptr(), 2 | 0x100) };
        if library.is_null() {
            return Err(UNAVAILABLE);
        }
        let init: Init = unsafe { symbol(library, c"km_init")? };
        let this = Self {
            close: unsafe { symbol(library, c"km_close")? },
            generate: unsafe { symbol(library, c"km_generate_key")? },
            characteristics: unsafe { symbol(library, c"km_get_key_characteristics")? },
            export: unsafe { symbol(library, c"km_export_key")? },
            begin: unsafe { symbol(library, c"km_begin")? },
            finish: unsafe { symbol(library, c"km_finish")? },
            abort: unsafe { symbol(library, c"km_abort")? },
        };
        // Compatibility inputs used by the native library's existing client;
        // these are NOT authenticated boot state or an attestation statement.
        let context = [0x00ef_200a_u64, 0, 0, 0];
        let context = Blob {
            data: context.as_ptr().cast_mut().cast(),
            size: 32,
        };
        status(unsafe { init(&context, 110000, 202103) })?;
        Ok(this)
    }
    fn check(&self, key: &Blob) -> Result<(), i32> {
        let mut chars = Characteristics {
            hardware: Params::empty(),
            software: Params::empty(),
        };
        status(unsafe { (self.characteristics)(key, ptr::null(), ptr::null(), &mut chars) })?;
        let hw = &chars.hardware;
        if hw.values.is_null() || hw.count == 0 || hw.count > 256 {
            return Err(WRONG_KEY);
        }
        let params = unsafe { std::slice::from_raw_parts(hw.values, hw.count) };
        for (tag, value) in [
            (0x10000002, 3),
            (0x30000003, 256),
            (0x20000005, 4),
            (0x20000001, 2),
            (0x100002be, 0),
        ] {
            if !params
                .iter()
                .any(|p| p.tag == tag && unsafe { p.value.num } == value)
            {
                return Err(WRONG_KEY);
            }
        }
        Ok(())
    }
    fn public(&self, key: &Blob) -> Result<Vec<u8>, i32> {
        self.check(key)?;
        let mut public = Blob::empty();
        // Format 0 is X.509 SubjectPublicKeyInfo, never a private key.
        status(unsafe { (self.export)(0, key, ptr::null(), ptr::null(), &mut public) })?;
        public.copy(256)
    }
    fn create(&self) -> Result<(Vec<u8>, Vec<u8>), i32> {
        let mut values = [
            Param::number(0x10000002, 3),
            Param::number(0x30000003, 256),
            Param::number(0x20000005, 4),
            Param::number(0x20000001, 2),
            Param::number(0x20000001, 3),
        ];
        let params = Params::from(&mut values);
        let mut key = Blob::empty();
        let mut chars = Characteristics {
            hardware: Params::empty(),
            software: Params::empty(),
        };
        status(unsafe { (self.generate)(&params, &mut key, &mut chars) })?;
        let public = self.public(&key)?;
        Ok((key.copy(MAX_BLOB)?, public))
    }
    fn sign(&self, key: &Blob, message: &[u8]) -> Result<Vec<u8>, i32> {
        self.check(key)?;
        let mut values = [Param::number(0x20000005, 4)];
        let params = Params::from(&mut values);
        let mut out = Params::empty();
        let mut handle = 0;
        status(unsafe { (self.begin)(2, key, &params, &mut out, &mut handle) })?;
        let mut signature = Blob::empty();
        let result = unsafe {
            (self.finish)(
                handle,
                ptr::null(),
                &Blob::from(message),
                ptr::null(),
                &mut out,
                &mut signature,
            )
        };
        if result != 0 {
            let _ = unsafe { (self.abort)(handle) };
            return Err(result);
        }
        signature.copy(128)
    }
}
impl Drop for Keymaster {
    fn drop(&mut self) {
        let _ = unsafe { (self.close)() };
    }
}

fn input() -> Result<(u8, Vec<u8>, Vec<u8>), i32> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((MAX_BLOB + MAX_MESSAGE + 10) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| BAD_REQUEST)?;
    if bytes.len() < 9 {
        return Err(BAD_REQUEST);
    }
    let op = bytes[0];
    let key_len = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
    let message_len = u32::from_be_bytes(bytes[5..9].try_into().unwrap()) as usize;
    if key_len > MAX_BLOB || message_len > MAX_MESSAGE || bytes.len() != 9 + key_len + message_len {
        return Err(BAD_REQUEST);
    }
    match op {
        1 if key_len == 0 && message_len == 0 => (),
        2 if key_len > 0 && message_len == 0 => (),
        3 if key_len > 0 && message_len > 0 => (),
        _ => return Err(BAD_REQUEST),
    }
    Ok((
        op,
        bytes[9..9 + key_len].to_vec(),
        bytes[9 + key_len..].to_vec(),
    ))
}

fn main() {
    if unsafe { setrlimit(4, &[0, 0]) } != 0 {
        std::process::exit(1);
    }
    // Preserve the private protocol pipe, then discard native-library stdout
    // and stderr. No blob or user data is ever formatted into diagnostic logs.
    let fd = unsafe { dup(1) };
    if fd < 0 {
        std::process::exit(1);
    }
    // Do not leak the protocol pipe if a native library executes a child.
    if unsafe { fcntl(fd, 2, 1) } < 0 {
        std::process::exit(1);
    }
    let mut output = unsafe { File::from_raw_fd(fd) };
    let null = File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .unwrap();
    unsafe {
        dup2(null.as_raw_fd(), 1);
        dup2(null.as_raw_fd(), 2);
        alarm(10);
    }
    let result = input().and_then(|(op, key, message)| {
        let km = Keymaster::open()?;
        match op {
            1 => km.create(),
            2 => Ok((Vec::new(), km.public(&Blob::from(&key))?)),
            3 => Ok((Vec::new(), km.sign(&Blob::from(&key), &message)?)),
            _ => Err(BAD_REQUEST),
        }
    });
    let (code, first, second) = match result {
        Ok((a, b)) => (0, a, b),
        Err(code) => (code, Vec::new(), Vec::new()),
    };
    let mut frame = b"DDKI1".to_vec();
    frame.extend_from_slice(&code.to_be_bytes());
    frame.extend_from_slice(&(first.len() as u32).to_be_bytes());
    frame.extend_from_slice(&(second.len() as u32).to_be_bytes());
    frame.extend_from_slice(&first);
    frame.extend_from_slice(&second);
    let _ = output.write_all(&frame);
    // This deliberately short-lived process reclaims all native allocations,
    // including old firmware without keymaster_free_* exported functions.
}
