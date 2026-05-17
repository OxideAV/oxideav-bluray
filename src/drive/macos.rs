//! macOS AACS Volume Identifier reader via IOKit + SCSITaskDevice.
//!
//! Loads `IOKit.framework` and `CoreFoundation.framework` at runtime via
//! [`libloading`] (matches oxideplay's SDL2 loader pattern: no link-time
//! framework dep). The dispatch is COM-style: most of the work goes
//! through vtables of function pointers reached via double-indirection
//! (`**plugin`, `**interface`).
//!
//! Flow:
//!
//! 1. `statfs(disc_root)` → `f_mntfromname` (`/dev/disk4s1`) → strip the
//!    partition-slice suffix → `disk4` (BSD whole-disk name).
//! 2. Walk the IORegistry for services matching `IOBDServices` (and fall
//!    back to `IODVDServices` — macOS still registers Blu-ray drives
//!    under the DVD-services class on some host controllers). Match each
//!    iterator entry by reading its `BSD Name` property, recursing into
//!    children (the optical-drive service publishes its BSD Name on a
//!    child IOMedia entry, not on itself).
//! 3. `IOCreatePlugInInterfaceForService(service,
//!    kIOSCSITaskDeviceUserClientTypeID, kIOCFPlugInInterfaceID, …)` →
//!    `QueryInterface(plugin, kIOSCSITaskDeviceInterfaceID, …)` →
//!    `SCSITaskDeviceInterface**`.
//! 4. `ObtainExclusiveAccess` → `CreateSCSITask` →
//!    `SetCommandDescriptorBlock` (12-byte READ DISC STRUCTURE CDB) →
//!    `SetScatterGatherEntries` (one IOAddressRange covering a 36-byte
//!    DMA buffer, direction = FromTargetToInitiator) →
//!    `SetTimeoutDuration(5000ms)` → `ExecuteTaskSync`.
//! 5. Parse: 4-byte response header (length + reserved) followed by the
//!    16-byte Volume Identifier at offsets 4..20.
//! 6. Release the task, release exclusive access, drop the plugin.
//!
//! ## Exclusive access caveat
//!
//! `ObtainExclusiveAccess` returns `kIOReturnExclusiveAccess` when the
//! volume is still mounted by Finder. The fix is to unmount the volume
//! (but leave the disc inserted) with `diskutil unmount /Volumes/<name>`
//! before re-running. The mount-point path itself is no longer
//! resolvable after unmount, so callers that want to use this code path
//! against a freshly-unmounted disc should capture the BSD device path
//! up front (or use `OXIDEAV_AACS_VOLUME_ID=<32-hex chars>` as an
//! override).
//!
//! ## Clean-room references
//!
//! Only Apple's public SDK headers (shipping with Xcode Command Line
//! Tools under the Apple Public Source License) were consulted to
//! derive the struct layouts and constants:
//!
//! - `IOKit.framework/Headers/IOCFPlugIn.h`
//! - `IOKit.framework/Headers/IOKitLib.h`
//! - `IOKit.framework/Headers/IOTypes.h`
//! - `IOKit.framework/Headers/IOReturn.h`
//! - `IOKit.framework/Headers/IOBSD.h`
//! - `IOKit.framework/Headers/scsi/SCSITaskLib.h`
//! - `IOKit.framework/Headers/scsi/SCSITask.h`
//! - `IOKit.framework/Headers/scsi/SCSICmds_REQUEST_SENSE_Defs.h`
//! - `CoreFoundation.framework/Headers/CFBase.h`
//! - `CoreFoundation.framework/Headers/CFString.h`
//! - `CoreFoundation.framework/Headers/CFUUID.h`
//! - `CoreFoundation.framework/Headers/CFPlugInCOM.h`
//!
//! No third-party Blu-ray decryption code (libbluray, libaacs,
//! aacskeys, makemkv, AnyDVD, …) was consulted.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
// CoreFoundation / IOKit typedefs follow Apple's all-caps convention
// (HRESULT, ULONG, IOReturn, kSCSITaskStatus_GOOD …) — matching the SDK
// header names keeps the FFI boundary auditable against
// /Library/Developer/CommandLineTools/SDKs/…/IOKit.framework/Headers.
#![allow(clippy::upper_case_acronyms)]
// IOKit + CoreFoundation are a C COM-style vtable API; every callable
// in this module is `unsafe`. The lib-root lint is `deny(unsafe_code)`
// rather than `forbid` precisely so this single module can opt in.
#![allow(unsafe_code)]

use super::DriveError;
use std::ffi::{c_char, c_int, c_void, CString};
use std::path::Path;
use std::ptr;

use libloading::{Library, Symbol};

// ---------------------------------------------------------------------------
// IOKit / CoreFoundation FFI types
// ---------------------------------------------------------------------------

// `mach_port_t` is `unsigned int` in user space on every Darwin target
// we ship. All `io_*_t` typedefs collapse to this.
type mach_port_t = u32;
type io_object_t = mach_port_t;
type io_service_t = io_object_t;
type io_iterator_t = io_object_t;
type io_registry_entry_t = io_object_t;
type kern_return_t = i32;
type IOReturn = kern_return_t;
type SInt32 = i32;
type HRESULT = i32;
type ULONG = u32;
type Boolean = u8;
type UInt8 = u8;
type UInt16 = u16;
type UInt32 = u32;
type UInt64 = u64;
type IOOptionBits = u32;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type CFStringEncoding = u32;

const kCFStringEncodingUTF8: CFStringEncoding = 0x0800_0100;
const kIOReturnSuccess: IOReturn = 0;
// `iokit_common_err(0x2c5)` = sys_iokit(0x38) << 26 | sub_iokit_common(0)
// | 0x2c5. We surface the bit pattern by name only so we can give a
// targeted error message; the actual numeric value is only logged.
const kIOReturnExclusiveAccess: IOReturn = 0xE00_002C5u32 as i32;
const kSCSITaskStatus_GOOD: u32 = 0x00;
const kSCSITaskStatus_CHECK_CONDITION: u32 = 0x02;
const kSCSIDataTransfer_FromTargetToInitiator: u8 = 0x02;
const kIORegistryIterateRecursively: IOOptionBits = 0x0000_0001;

/// 128-bit raw UUID bytes laid out exactly as CoreFoundation expects
/// (`CFUUIDBytes` from `CFUUID.h`).
#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes {
    bytes: [u8; 16],
}

impl CFUUIDBytes {
    const fn new(b: [u8; 16]) -> Self {
        Self { bytes: b }
    }
}

/// `IOAddressRange` (= `IOVirtualRange` on LP64 — both fields are 64-bit)
/// from `IOKit/IOTypes.h`. This is `SCSITaskSGElement` on LP64 macOS.
#[repr(C)]
#[derive(Clone, Copy)]
struct IOAddressRange {
    address: u64,
    length: u64,
}

/// `SCSI_Sense_Data` from `SCSICmds_REQUEST_SENSE_Defs.h`. 18 bytes,
/// `kSenseDefaultSize`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SCSI_Sense_Data {
    raw: [u8; 18],
}

impl SCSI_Sense_Data {
    const fn zeroed() -> Self {
        Self { raw: [0u8; 18] }
    }
    fn sense_key(&self) -> u8 {
        self.raw[2] & 0x0F
    }
    fn asc(&self) -> u8 {
        self.raw[12]
    }
    fn ascq(&self) -> u8 {
        self.raw[13]
    }
}

// `IUNKNOWN_C_GUTS` from `CFPlugInCOM.h` expands to four pointer-sized
// fields: _reserved, QueryInterface, AddRef, Release. The IOKit plugin
// adds `version, revision, Probe, Start, Stop`. The SCSITask interfaces
// add `version, revision` then their own method table.

type Fn_QueryInterface =
    unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> HRESULT;
type Fn_AddRef = unsafe extern "C" fn(*mut c_void) -> ULONG;
type Fn_Release = unsafe extern "C" fn(*mut c_void) -> ULONG;

/// `IOCFPlugInInterface` vtable from `IOCFPlugIn.h`. Only the first
/// three IUnknown slots are exercised here (we never call `Probe` /
/// `Start` / `Stop` — `QueryInterface` is the only path to the
/// SCSITaskDeviceInterface vtable we actually need).
#[repr(C)]
struct IOCFPlugInInterface {
    _reserved: *mut c_void,
    QueryInterface: Fn_QueryInterface,
    AddRef: Fn_AddRef,
    Release: Fn_Release,
    version: UInt16,
    revision: UInt16,
    Probe: *mut c_void,
    Start: *mut c_void,
    Stop: *mut c_void,
}

/// `SCSITaskDeviceInterface` vtable from `SCSITaskLib.h`. We only call
/// the four highlighted methods — the others are kept for layout
/// fidelity so the field offsets line up with the C struct.
#[repr(C)]
struct SCSITaskDeviceInterface {
    _reserved: *mut c_void,
    QueryInterface: Fn_QueryInterface,
    AddRef: Fn_AddRef,
    Release: Fn_Release,
    version: UInt16,
    revision: UInt16,
    IsExclusiveAccessAvailable: unsafe extern "C" fn(*mut c_void) -> Boolean,
    AddCallbackDispatcherToRunLoop: unsafe extern "C" fn(*mut c_void, *mut c_void) -> IOReturn,
    RemoveCallbackDispatcherFromRunLoop: unsafe extern "C" fn(*mut c_void),
    ObtainExclusiveAccess: unsafe extern "C" fn(*mut c_void) -> IOReturn,
    ReleaseExclusiveAccess: unsafe extern "C" fn(*mut c_void) -> IOReturn,
    CreateSCSITask: unsafe extern "C" fn(*mut c_void) -> *mut *mut SCSITaskInterface,
}

/// `SCSITaskInterface` vtable from `SCSITaskLib.h`. The
/// `SetTaskCompletionCallback` / `ExecuteTaskAsync` / `AbortTask` slots
/// are kept (typed as opaque function pointers) so subsequent fields
/// line up at the correct offsets; only the synchronous-execution slots
/// are reachable through the typed methods below.
#[repr(C)]
struct SCSITaskInterface {
    _reserved: *mut c_void,
    QueryInterface: Fn_QueryInterface,
    AddRef: Fn_AddRef,
    Release: Fn_Release,
    version: UInt16,
    revision: UInt16,
    IsTaskActive: unsafe extern "C" fn(*mut c_void) -> Boolean,
    SetTaskAttribute: unsafe extern "C" fn(*mut c_void, UInt32) -> IOReturn,
    GetTaskAttribute: unsafe extern "C" fn(*mut c_void, *mut UInt32) -> IOReturn,
    SetCommandDescriptorBlock: unsafe extern "C" fn(*mut c_void, *const UInt8, UInt8) -> IOReturn,
    GetCommandDescriptorBlockSize: unsafe extern "C" fn(*mut c_void) -> UInt8,
    GetCommandDescriptorBlock: unsafe extern "C" fn(*mut c_void, *mut UInt8) -> IOReturn,
    SetScatterGatherEntries:
        unsafe extern "C" fn(*mut c_void, *const IOAddressRange, UInt8, UInt64, UInt8) -> IOReturn,
    SetTimeoutDuration: unsafe extern "C" fn(*mut c_void, UInt32) -> IOReturn,
    GetTimeoutDuration: unsafe extern "C" fn(*mut c_void) -> UInt32,
    SetTaskCompletionCallback: *mut c_void,
    ExecuteTaskAsync: *mut c_void,
    ExecuteTaskSync: unsafe extern "C" fn(
        *mut c_void,
        *mut SCSI_Sense_Data,
        *mut UInt32,
        *mut UInt64,
    ) -> IOReturn,
}

// ---------------------------------------------------------------------------
// COM-style UUIDs from SCSITaskLib.h and IOCFPlugIn.h
// ---------------------------------------------------------------------------

// kIOCFPlugInInterfaceID = C244E858-109C-11D4-91D4-0050E4C6426F
const K_IO_CF_PLUGIN_INTERFACE_ID: CFUUIDBytes = CFUUIDBytes::new([
    0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F,
]);

// kIOSCSITaskDeviceUserClientTypeID = 7D66678E-08A2-11D5-A1B8-0030657D052A
const K_IO_SCSI_TASK_DEVICE_USER_CLIENT_TYPE_ID: CFUUIDBytes = CFUUIDBytes::new([
    0x7D, 0x66, 0x67, 0x8E, 0x08, 0xA2, 0x11, 0xD5, 0xA1, 0xB8, 0x00, 0x30, 0x65, 0x7D, 0x05, 0x2A,
]);

// kIOSCSITaskDeviceInterfaceID = 1BBC4132-08A5-11D5-90ED-0030657D052A
const K_IO_SCSI_TASK_DEVICE_INTERFACE_ID: CFUUIDBytes = CFUUIDBytes::new([
    0x1B, 0xBC, 0x41, 0x32, 0x08, 0xA5, 0x11, 0xD5, 0x90, 0xED, 0x00, 0x30, 0x65, 0x7D, 0x05, 0x2A,
]);

// ---------------------------------------------------------------------------
// Resolved FFI symbols
// ---------------------------------------------------------------------------

type Fn_IOServiceMatching = unsafe extern "C" fn(*const c_char) -> CFTypeRef;
type Fn_IOServiceGetMatchingServices =
    unsafe extern "C" fn(mach_port_t, CFTypeRef, *mut io_iterator_t) -> kern_return_t;
type Fn_IOIteratorNext = unsafe extern "C" fn(io_iterator_t) -> io_object_t;
type Fn_IOObjectRelease = unsafe extern "C" fn(io_object_t) -> kern_return_t;
type Fn_IORegistryEntrySearchCFProperty = unsafe extern "C" fn(
    io_registry_entry_t,
    *const c_char,
    CFStringRef,
    CFAllocatorRef,
    IOOptionBits,
) -> CFTypeRef;
type Fn_IOCreatePlugInInterfaceForService = unsafe extern "C" fn(
    io_service_t,
    CFTypeRef,
    CFTypeRef,
    *mut *mut *mut IOCFPlugInInterface,
    *mut SInt32,
) -> kern_return_t;
type Fn_IODestroyPlugInInterface =
    unsafe extern "C" fn(*mut *mut IOCFPlugInInterface) -> kern_return_t;
type Fn_IORegistryEntryGetParentEntry = unsafe extern "C" fn(
    io_registry_entry_t,
    *const c_char,
    *mut io_registry_entry_t,
) -> kern_return_t;
type Fn_CFRelease = unsafe extern "C" fn(CFTypeRef);
type Fn_CFStringCreateWithCString =
    unsafe extern "C" fn(CFAllocatorRef, *const c_char, CFStringEncoding) -> CFStringRef;
type Fn_CFStringGetCString =
    unsafe extern "C" fn(CFStringRef, *mut c_char, CFIndex, CFStringEncoding) -> Boolean;
type Fn_CFGetTypeID = unsafe extern "C" fn(CFTypeRef) -> usize;
type Fn_CFStringGetTypeID = unsafe extern "C" fn() -> usize;
type Fn_CFUUIDCreateFromUUIDBytes = unsafe extern "C" fn(CFAllocatorRef, CFUUIDBytes) -> CFTypeRef;

struct Symbols {
    _iokit: Library,
    _corefoundation: Library,

    IOServiceMatching: Fn_IOServiceMatching,
    IOServiceGetMatchingServices: Fn_IOServiceGetMatchingServices,
    IOIteratorNext: Fn_IOIteratorNext,
    IOObjectRelease: Fn_IOObjectRelease,
    IORegistryEntrySearchCFProperty: Fn_IORegistryEntrySearchCFProperty,
    IOCreatePlugInInterfaceForService: Fn_IOCreatePlugInInterfaceForService,
    IODestroyPlugInInterface: Fn_IODestroyPlugInInterface,
    IORegistryEntryGetParentEntry: Fn_IORegistryEntryGetParentEntry,

    CFRelease: Fn_CFRelease,
    CFStringCreateWithCString: Fn_CFStringCreateWithCString,
    CFStringGetCString: Fn_CFStringGetCString,
    CFGetTypeID: Fn_CFGetTypeID,
    CFStringGetTypeID: Fn_CFStringGetTypeID,
    CFUUIDCreateFromUUIDBytes: Fn_CFUUIDCreateFromUUIDBytes,
}

impl Symbols {
    fn load() -> Result<Self, DriveError> {
        // SAFETY: paths are constant, well-known system frameworks.
        // libloading::Library::new is the documented way to dlopen.
        unsafe {
            let iokit = Library::new("/System/Library/Frameworks/IOKit.framework/IOKit")
                .map_err(|e| DriveError::Mmc(format!("couldn't load IOKit.framework: {e}")))?;
            let cf =
                Library::new("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
                    .map_err(|e| {
                        DriveError::Mmc(format!("couldn't load CoreFoundation.framework: {e}"))
                    })?;

            // Helper closure that retrieves a Symbol and dereferences it
            // to a fn pointer of the requested type. The symbol borrows
            // from the library; we copy the underlying fn pointer out
            // and let the library handle survive in the returned struct.
            fn get<T: Copy>(
                lib: &Library,
                name: &[u8],
                what: &'static str,
            ) -> Result<T, DriveError> {
                // SAFETY: we trust the system framework's exported
                // symbol layout to match the typed fn pointer above;
                // verified against the corresponding SDK header.
                let sym: Symbol<T> = unsafe { lib.get(name) }
                    .map_err(|e| DriveError::Mmc(format!("missing symbol {what}: {e}")))?;
                Ok(*sym)
            }

            Ok(Symbols {
                IOServiceMatching: get(&iokit, b"IOServiceMatching\0", "IOServiceMatching")?,
                IOServiceGetMatchingServices: get(
                    &iokit,
                    b"IOServiceGetMatchingServices\0",
                    "IOServiceGetMatchingServices",
                )?,
                IOIteratorNext: get(&iokit, b"IOIteratorNext\0", "IOIteratorNext")?,
                IOObjectRelease: get(&iokit, b"IOObjectRelease\0", "IOObjectRelease")?,
                IORegistryEntrySearchCFProperty: get(
                    &iokit,
                    b"IORegistryEntrySearchCFProperty\0",
                    "IORegistryEntrySearchCFProperty",
                )?,
                IOCreatePlugInInterfaceForService: get(
                    &iokit,
                    b"IOCreatePlugInInterfaceForService\0",
                    "IOCreatePlugInInterfaceForService",
                )?,
                IODestroyPlugInInterface: get(
                    &iokit,
                    b"IODestroyPlugInInterface\0",
                    "IODestroyPlugInInterface",
                )?,
                IORegistryEntryGetParentEntry: get(
                    &iokit,
                    b"IORegistryEntryGetParentEntry\0",
                    "IORegistryEntryGetParentEntry",
                )?,

                CFRelease: get(&cf, b"CFRelease\0", "CFRelease")?,
                CFStringCreateWithCString: get(
                    &cf,
                    b"CFStringCreateWithCString\0",
                    "CFStringCreateWithCString",
                )?,
                CFStringGetCString: get(&cf, b"CFStringGetCString\0", "CFStringGetCString")?,
                CFGetTypeID: get(&cf, b"CFGetTypeID\0", "CFGetTypeID")?,
                CFStringGetTypeID: get(&cf, b"CFStringGetTypeID\0", "CFStringGetTypeID")?,
                CFUUIDCreateFromUUIDBytes: get(
                    &cf,
                    b"CFUUIDCreateFromUUIDBytes\0",
                    "CFUUIDCreateFromUUIDBytes",
                )?,
                _iokit: iokit,
                _corefoundation: cf,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// statfs FFI (just enough to read `f_mntfromname`)
// ---------------------------------------------------------------------------

// `__DARWIN_STRUCT_STATFS64` layout from `sys/mount.h`. We never look
// past `f_mntfromname` so the trailing reserved fields would be
// harmless to omit, but keeping the full size matches what the kernel
// expects to write through the pointer.
#[repr(C)]
struct Statfs {
    f_bsize: u32,
    f_iosize: i32,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_fsid: [i32; 2],
    f_owner: u32,
    f_type: u32,
    f_flags: u32,
    f_fssubtype: u32,
    f_fstypename: [c_char; 16],
    f_mntonname: [c_char; 1024],
    f_mntfromname: [c_char; 1024],
    f_flags_ext: u32,
    f_reserved: [u32; 7],
}

extern "C" {
    // Modern 64-bit-inode statfs. On arm64 Darwin the unsuffixed symbol
    // resolves directly; on x86_64 the C compiler rewrites `statfs` to
    // `statfs$INODE64` when `_DARWIN_USE_64_BIT_INODE` is set (which is
    // the default since 10.6). The libloading approach for IOKit doesn't
    // help us here because `libc` is part of the platform — we just
    // declare the right symbol name per arch.
    #[cfg(target_arch = "x86_64")]
    #[link_name = "statfs$INODE64"]
    fn statfs(path: *const c_char, buf: *mut Statfs) -> c_int;
    #[cfg(not(target_arch = "x86_64"))]
    fn statfs(path: *const c_char, buf: *mut Statfs) -> c_int;

    fn __error() -> *mut c_int;
}

fn errno() -> c_int {
    // SAFETY: __error returns a thread-local pointer that's always
    // valid for the calling thread's lifetime.
    unsafe { *__error() }
}

/// Returns the BSD whole-disk name (e.g. `disk4`) for the mount point
/// `disc_root`. Strips the slice suffix (`s1`, `s2`, …) so callers get
/// the disk-level node the SCSITaskDevice plugin attaches to, not the
/// per-partition IOMedia child.
fn bsd_name_for_mount(disc_root: &Path) -> Result<String, DriveError> {
    let c = CString::new(disc_root.as_os_str().to_string_lossy().as_bytes()).map_err(|_| {
        DriveError::Mmc(format!("mount path {disc_root:?} contains an interior NUL"))
    })?;

    // Zero-init the whole struct — we only consume f_mntfromname.
    let mut buf: Statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { statfs(c.as_ptr(), &mut buf as *mut Statfs) };
    if rc != 0 {
        return Err(DriveError::Mmc(format!(
            "statfs({}) failed: errno {}",
            disc_root.display(),
            errno()
        )));
    }

    // f_mntfromname is e.g. "/dev/disk4s1\0…"
    let mntfrom = unsafe { std::ffi::CStr::from_ptr(buf.f_mntfromname.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    // Strip "/dev/" prefix if present.
    let bsd = mntfrom.strip_prefix("/dev/").unwrap_or(&mntfrom);

    // Strip slice suffix: walk back from the end while we see digits,
    // then if we now see 's', drop from there. Handles "disk4s1" -> "disk4"
    // and "disk4s1s2" (APFS synthesised) -> "disk4". Whole disks like
    // "disk4" with no slice are returned unchanged.
    let bytes = bsd.as_bytes();
    let mut end = bytes.len();
    loop {
        let mut i = end;
        while i > 0 && bytes[i - 1].is_ascii_digit() {
            i -= 1;
        }
        if i > 0 && i < end && bytes[i - 1] == b's' {
            end = i - 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return Err(DriveError::Mmc(format!(
            "couldn't strip slice from BSD name {bsd:?}"
        )));
    }
    Ok(bsd[..end].to_string())
}

// ---------------------------------------------------------------------------
// IORegistry walk
// ---------------------------------------------------------------------------

/// `CFRelease`-on-drop guard so error paths can't leak the CF object.
struct CfRef<'a> {
    syms: &'a Symbols,
    ptr: CFTypeRef,
}

impl Drop for CfRef<'_> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: ptr originates from a CF Create / Get-with-retain
            // call; we own exactly one reference.
            unsafe { (self.syms.CFRelease)(self.ptr) }
        }
    }
}

/// `IOObjectRelease`-on-drop guard for `io_object_t` handles.
struct IoObject<'a> {
    syms: &'a Symbols,
    obj: io_object_t,
}

impl Drop for IoObject<'_> {
    fn drop(&mut self) {
        if self.obj != 0 {
            // SAFETY: obj comes from IOServiceGetMatchingServices /
            // IOIteratorNext and was not released elsewhere.
            unsafe {
                (self.syms.IOObjectRelease)(self.obj);
            }
        }
    }
}

fn cf_string(syms: &Symbols, s: &str) -> Result<*const c_void, DriveError> {
    let c = CString::new(s)
        .map_err(|_| DriveError::Mmc(format!("string {s:?} contains an interior NUL")))?;
    // SAFETY: c.as_ptr() is valid for the lifetime of c; allocator is
    // the default; encoding is UTF-8.
    let r =
        unsafe { (syms.CFStringCreateWithCString)(ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) };
    if r.is_null() {
        return Err(DriveError::Mmc(format!(
            "CFStringCreateWithCString({s:?}) returned NULL"
        )));
    }
    Ok(r)
}

/// Read the `BSD Name` string property from a registry entry, searching
/// recursively into children so we match the IOMedia descendant of an
/// IOBDServices / IODVDServices entry.
fn entry_bsd_name(syms: &Symbols, entry: io_registry_entry_t) -> Option<String> {
    // SAFETY: literal C string, immortal storage.
    let key = match cf_string(syms, "BSD Name") {
        Ok(k) => CfRef { syms, ptr: k },
        Err(_) => return None,
    };
    // SAFETY: "IOService" is the canonical plane name from IOKitKeys.h.
    let plane = b"IOService\0";
    let val = unsafe {
        (syms.IORegistryEntrySearchCFProperty)(
            entry,
            plane.as_ptr() as *const c_char,
            key.ptr,
            ptr::null(),
            kIORegistryIterateRecursively,
        )
    };
    if val.is_null() {
        return None;
    }
    let cf_val = CfRef { syms, ptr: val };

    // Confirm it's actually a CFString (sometimes BSD Name lookups
    // return CFArray etc. on multi-link entries). Compare type IDs.
    let got = unsafe { (syms.CFGetTypeID)(cf_val.ptr) };
    let want = unsafe { (syms.CFStringGetTypeID)() };
    if got != want {
        return None;
    }

    let mut buf = [0u8; 256];
    let ok = unsafe {
        (syms.CFStringGetCString)(
            cf_val.ptr,
            buf.as_mut_ptr() as *mut c_char,
            buf.len() as CFIndex,
            kCFStringEncodingUTF8,
        )
    };
    if ok == 0 {
        return None;
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..nul]).into_owned())
}

/// Walk IORegistry services of the named class, returning the
/// `io_service_t` whose `BSD Name` (anywhere in the child plane) matches
/// `bsd_name`. The returned service is an owned handle — drop with
/// `IOObjectRelease`.
fn find_service_by_class_and_bsd(
    syms: &Symbols,
    class_name: &str,
    bsd_name: &str,
) -> Result<Option<io_service_t>, DriveError> {
    let class_c = CString::new(class_name).map_err(|_| {
        DriveError::Mmc(format!(
            "class name {class_name:?} contains an interior NUL"
        ))
    })?;
    // IOServiceMatching consumes one reference on success via the next
    // call to IOServiceGetMatchingServices, so we do NOT wrap it in a
    // CfRef on the success path. On failure (NULL return) there's
    // nothing to release.
    let matching = unsafe { (syms.IOServiceMatching)(class_c.as_ptr()) };
    if matching.is_null() {
        return Ok(None);
    }

    let mut iter: io_iterator_t = 0;
    let kr = unsafe { (syms.IOServiceGetMatchingServices)(0, matching, &mut iter) };
    if kr != kIOReturnSuccess {
        return Err(DriveError::Mmc(format!(
            "IOServiceGetMatchingServices({class_name}) failed: 0x{kr:08x}"
        )));
    }
    let iter_guard = IoObject { syms, obj: iter };

    loop {
        let svc = unsafe { (syms.IOIteratorNext)(iter_guard.obj) };
        if svc == 0 {
            return Ok(None);
        }
        let svc_guard = IoObject { syms, obj: svc };
        if let Some(name) = entry_bsd_name(syms, svc) {
            if name == bsd_name {
                // Hand ownership of the io_service_t back to the caller.
                let owned = svc_guard.obj;
                std::mem::forget(svc_guard);
                return Ok(Some(owned));
            }
        }
    }
}

fn find_optical_service(syms: &Symbols, bsd_name: &str) -> Result<io_service_t, DriveError> {
    for class in ["IOBDServices", "IODVDServices"] {
        if let Some(s) = find_service_by_class_and_bsd(syms, class, bsd_name)? {
            return Ok(s);
        }
    }
    Err(DriveError::Mmc(format!(
        "couldn't find optical drive service matching BSD name {bsd_name:?} \
         (tried IOBDServices and IODVDServices)"
    )))
}

// ---------------------------------------------------------------------------
// SCSITaskDevice plumbing
// ---------------------------------------------------------------------------

/// Owns a `**SCSITaskDeviceInterface` handle and releases exclusive
/// access + the interface on drop.
struct ScsiDevice<'a> {
    syms: &'a Symbols,
    /// Double-indirection per IOKit COM convention.
    iface: *mut *mut SCSITaskDeviceInterface,
    plugin: *mut *mut IOCFPlugInInterface,
    have_excl: bool,
}

impl Drop for ScsiDevice<'_> {
    fn drop(&mut self) {
        unsafe {
            if self.have_excl && !self.iface.is_null() {
                ((**self.iface).ReleaseExclusiveAccess)(self.iface as *mut c_void);
            }
            if !self.iface.is_null() {
                ((**self.iface).Release)(self.iface as *mut c_void);
            }
            if !self.plugin.is_null() {
                (self.syms.IODestroyPlugInInterface)(self.plugin);
            }
        }
    }
}

fn open_scsi_device<'a>(
    syms: &'a Symbols,
    service: io_service_t,
) -> Result<ScsiDevice<'a>, DriveError> {
    // Wrap UUIDs into CFUUIDRef.
    let plugin_uuid =
        unsafe { (syms.CFUUIDCreateFromUUIDBytes)(ptr::null(), K_IO_CF_PLUGIN_INTERFACE_ID) };
    if plugin_uuid.is_null() {
        return Err(DriveError::Mmc(
            "CFUUIDCreateFromUUIDBytes(kIOCFPlugInInterfaceID) returned NULL".to_string(),
        ));
    }
    let plugin_uuid = CfRef {
        syms,
        ptr: plugin_uuid,
    };

    let dev_type_uuid = unsafe {
        (syms.CFUUIDCreateFromUUIDBytes)(ptr::null(), K_IO_SCSI_TASK_DEVICE_USER_CLIENT_TYPE_ID)
    };
    if dev_type_uuid.is_null() {
        return Err(DriveError::Mmc(
            "CFUUIDCreateFromUUIDBytes(kIOSCSITaskDeviceUserClientTypeID) returned NULL"
                .to_string(),
        ));
    }
    let dev_type_uuid = CfRef {
        syms,
        ptr: dev_type_uuid,
    };

    // The SCSITaskDeviceUserClient plugin factory is registered on the
    // IOSCSIPeripheralDeviceTypeXX node (the SCSI-level nub), not on
    // the IOBDServices / IODVDServices media-class node we matched via
    // BSD name. Try the matched service first; if it answers
    // `kIOReturnUnsupported`, walk up the IOService plane to the SCSI
    // peripheral parent and retry. Bounded by IOREG_MAX_PARENT_HOPS so
    // we never loop indefinitely on a pathological registry.
    const IOREG_MAX_PARENT_HOPS: usize = 6;
    let mut plugin: *mut *mut IOCFPlugInInterface = ptr::null_mut();
    let mut score: SInt32 = 0;
    let mut current = service;
    // Owned io_objects we created via IORegistryEntryGetParentEntry —
    // we must release each before returning. The originally-passed
    // `service` is the caller's; we don't release it.
    let mut owned_parents: Vec<io_registry_entry_t> = Vec::new();
    let plane = b"IOService\0";
    let mut last_kr: kern_return_t = kIOReturnSuccess;
    for hop in 0..=IOREG_MAX_PARENT_HOPS {
        let kr = unsafe {
            (syms.IOCreatePlugInInterfaceForService)(
                current,
                dev_type_uuid.ptr,
                plugin_uuid.ptr,
                &mut plugin,
                &mut score,
            )
        };
        last_kr = kr;
        if kr == kIOReturnSuccess && !plugin.is_null() {
            break;
        }
        // Climb to parent for the next attempt.
        let mut parent: io_registry_entry_t = 0;
        let pkr = unsafe {
            (syms.IORegistryEntryGetParentEntry)(
                current,
                plane.as_ptr() as *const c_char,
                &mut parent,
            )
        };
        if pkr != kIOReturnSuccess || parent == 0 {
            for p in &owned_parents {
                unsafe { (syms.IOObjectRelease)(*p) };
            }
            return Err(DriveError::Mmc(format!(
                "IOCreatePlugInInterfaceForService(kIOSCSITaskDeviceUserClientTypeID) \
                 failed at every parent hop (0..={hop}): last status 0x{kr:08x}; \
                 final parent walk: 0x{pkr:08x}"
            )));
        }
        owned_parents.push(parent);
        current = parent;
    }
    // Release the parent chain — IOCreatePlugInInterfaceForService
    // doesn't retain the service, so the io_object_t is no longer
    // needed once the plugin handle is alive.
    for p in &owned_parents {
        unsafe { (syms.IOObjectRelease)(*p) };
    }
    if plugin.is_null() {
        return Err(DriveError::Mmc(format!(
            "IOCreatePlugInInterfaceForService(kIOSCSITaskDeviceUserClientTypeID) \
             returned NULL after walking up to {IOREG_MAX_PARENT_HOPS} parents; \
             last kern_return_t was 0x{last_kr:08x}"
        )));
    }

    // QueryInterface(plugin, kIOSCSITaskDeviceInterfaceID, &out)
    let mut iface: *mut c_void = ptr::null_mut();
    let hr = unsafe {
        ((**plugin).QueryInterface)(
            plugin as *mut c_void,
            K_IO_SCSI_TASK_DEVICE_INTERFACE_ID,
            &mut iface,
        )
    };
    if hr != 0 || iface.is_null() {
        unsafe { (syms.IODestroyPlugInInterface)(plugin) };
        return Err(DriveError::Mmc(format!(
            "QueryInterface(kIOSCSITaskDeviceInterfaceID) failed: 0x{hr:08x}"
        )));
    }
    let iface = iface as *mut *mut SCSITaskDeviceInterface;

    let mut dev = ScsiDevice {
        syms,
        iface,
        plugin,
        have_excl: false,
    };

    // Claim exclusive access — required before any SCSITask execution.
    let kr = unsafe { ((**iface).ObtainExclusiveAccess)(iface as *mut c_void) };
    if kr != kIOReturnSuccess {
        // Map the common "still mounted" failure to an actionable
        // message, surface everything else with the raw code.
        if kr == kIOReturnExclusiveAccess {
            return Err(DriveError::Mmc(format!(
                "ObtainExclusiveAccess failed (kIOReturnExclusiveAccess 0x{kr:08x}); \
                 the volume is still mounted by Finder — \
                 try `diskutil unmount /Volumes/<name>` first, then re-run"
            )));
        }
        return Err(DriveError::Mmc(format!(
            "ObtainExclusiveAccess failed: 0x{kr:08x}"
        )));
    }
    dev.have_excl = true;
    Ok(dev)
}

/// Issue the READ DISC STRUCTURE (opcode `0xAD`, format `0x80`) command
/// and return the 36-byte response buffer.
fn read_aacs_volume_id_buffer(dev: &ScsiDevice<'_>) -> Result<[u8; 36], DriveError> {
    // CDB per MMC-5 §6.21.4.4 + AACS Pre-recorded Video Book.
    let alloc_len: u16 = 36;
    let cdb: [u8; 12] = [
        0xAD, // operation code: READ DISC STRUCTURE
        0x01, // MEDIA TYPE = BD (4-bit field in low nibble)
        0x00,
        0x00,
        0x00,
        0x00,                     // Address (n/a for format 0x80)
        0x00,                     // Layer Number
        0x80,                     // Format: AACS Volume Identifier
        (alloc_len >> 8) as u8,   // Allocation Length MSB
        (alloc_len & 0xFF) as u8, // Allocation Length LSB
        0x00,                     // AGID (no auth)
        0x00,                     // Control
    ];

    let mut data = [0u8; 36];
    let mut sense = SCSI_Sense_Data::zeroed();
    let mut task_status: u32 = 0;
    let mut realized: u64 = 0;

    let task = unsafe { ((**dev.iface).CreateSCSITask)(dev.iface as *mut c_void) };
    if task.is_null() {
        return Err(DriveError::Mmc("CreateSCSITask returned NULL".to_string()));
    }
    // RAII guard for the SCSITask handle — Release on every exit path.
    struct TaskGuard<'a>(
        *mut *mut SCSITaskInterface,
        std::marker::PhantomData<&'a ()>,
    );
    impl Drop for TaskGuard<'_> {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { ((**self.0).Release)(self.0 as *mut c_void) };
            }
        }
    }
    let task_guard = TaskGuard(task, std::marker::PhantomData);

    let kr = unsafe {
        ((**task_guard.0).SetCommandDescriptorBlock)(
            task_guard.0 as *mut c_void,
            cdb.as_ptr(),
            cdb.len() as u8,
        )
    };
    if kr != kIOReturnSuccess {
        return Err(DriveError::Mmc(format!(
            "SetCommandDescriptorBlock failed: 0x{kr:08x}"
        )));
    }

    let sg = IOAddressRange {
        address: data.as_mut_ptr() as usize as u64,
        length: data.len() as u64,
    };
    let kr = unsafe {
        ((**task_guard.0).SetScatterGatherEntries)(
            task_guard.0 as *mut c_void,
            &sg,
            1,
            data.len() as u64,
            kSCSIDataTransfer_FromTargetToInitiator,
        )
    };
    if kr != kIOReturnSuccess {
        return Err(DriveError::Mmc(format!(
            "SetScatterGatherEntries failed: 0x{kr:08x}"
        )));
    }

    let kr = unsafe { ((**task_guard.0).SetTimeoutDuration)(task_guard.0 as *mut c_void, 5000) };
    if kr != kIOReturnSuccess {
        return Err(DriveError::Mmc(format!(
            "SetTimeoutDuration failed: 0x{kr:08x}"
        )));
    }

    let kr = unsafe {
        ((**task_guard.0).ExecuteTaskSync)(
            task_guard.0 as *mut c_void,
            &mut sense,
            &mut task_status,
            &mut realized,
        )
    };
    if kr != kIOReturnSuccess {
        return Err(DriveError::Mmc(format!(
            "ExecuteTaskSync (READ DISC STRUCTURE) failed: 0x{kr:08x}"
        )));
    }

    if task_status != kSCSITaskStatus_GOOD {
        if task_status == kSCSITaskStatus_CHECK_CONDITION {
            return Err(DriveError::Mmc(format!(
                "READ DISC STRUCTURE returned CHECK CONDITION (0x{:02x}), \
                 sense KCQ {:02x}/{:02x}/{:02x} — \
                 the drive may not support the AACS Volume Identifier format \
                 (0x80) on this medium",
                task_status,
                sense.sense_key(),
                sense.asc(),
                sense.ascq()
            )));
        }
        return Err(DriveError::Mmc(format!(
            "READ DISC STRUCTURE returned task status 0x{task_status:02x}"
        )));
    }

    if realized < 20 {
        return Err(DriveError::Mmc(format!(
            "READ DISC STRUCTURE transferred only {realized} bytes \
             (need at least 20: 4-byte header + 16-byte Volume Identifier)"
        )));
    }

    // Sanity-check the 2-byte big-endian response length in the header
    // — it counts bytes that *follow* the length field, so total bytes
    // present is header[0..2] + 2. The minimum we care about is 18
    // (2 bytes reserved + 16 bytes Volume Identifier).
    let resp_len = ((data[0] as u16) << 8) | (data[1] as u16);
    if (resp_len as usize) < 18 {
        return Err(DriveError::Mmc(format!(
            "READ DISC STRUCTURE response header reports only {resp_len} \
             bytes of payload (expected >= 18 for AACS Volume Identifier)"
        )));
    }

    Ok(data)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn read_volume_id(disc_root: &Path) -> Result<[u8; 16], DriveError> {
    let bsd = bsd_name_for_mount(disc_root)?;
    let syms = Symbols::load()?;
    let svc = find_optical_service(&syms, &bsd)?;
    let svc_guard = IoObject {
        syms: &syms,
        obj: svc,
    };
    let dev = open_scsi_device(&syms, svc_guard.obj)?;
    let buf = read_aacs_volume_id_buffer(&dev)?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&buf[4..20]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsd_name_strips_slice_suffix() {
        // Path-independent helper: replicate the slice-strip logic from
        // bsd_name_for_mount on a few known forms. We can't call
        // statfs() in CI (there's no Blu-ray mount), but the suffix
        // stripping is the only piece that's worth covering without
        // hardware. We replicate the inner block:
        fn strip(bsd: &str) -> Option<String> {
            let bytes = bsd.as_bytes();
            let mut end = bytes.len();
            loop {
                let mut i = end;
                while i > 0 && bytes[i - 1].is_ascii_digit() {
                    i -= 1;
                }
                if i > 0 && i < end && bytes[i - 1] == b's' {
                    end = i - 1;
                } else {
                    break;
                }
            }
            if end == 0 {
                None
            } else {
                Some(bsd[..end].to_string())
            }
        }

        assert_eq!(strip("disk4s1").as_deref(), Some("disk4"));
        assert_eq!(strip("disk4").as_deref(), Some("disk4"));
        assert_eq!(strip("disk12s3").as_deref(), Some("disk12"));
        assert_eq!(strip("disk4s1s2").as_deref(), Some("disk4"));
        assert_eq!(strip("disk10").as_deref(), Some("disk10"));
    }

    #[test]
    fn aacs_uuid_constants_match_sdk_header() {
        // Sanity-check the UUID byte arrays against the textual form in
        // SCSITaskLib.h. Off-by-one in this table would silently break
        // every QueryInterface call without obvious symptoms.
        assert_eq!(
            K_IO_CF_PLUGIN_INTERFACE_ID.bytes,
            [
                0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6,
                0x42, 0x6F,
            ]
        );
        assert_eq!(
            K_IO_SCSI_TASK_DEVICE_USER_CLIENT_TYPE_ID.bytes,
            [
                0x7D, 0x66, 0x67, 0x8E, 0x08, 0xA2, 0x11, 0xD5, 0xA1, 0xB8, 0x00, 0x30, 0x65, 0x7D,
                0x05, 0x2A,
            ]
        );
        assert_eq!(
            K_IO_SCSI_TASK_DEVICE_INTERFACE_ID.bytes,
            [
                0x1B, 0xBC, 0x41, 0x32, 0x08, 0xA5, 0x11, 0xD5, 0x90, 0xED, 0x00, 0x30, 0x65, 0x7D,
                0x05, 0x2A,
            ]
        );
    }

    #[test]
    fn statfs_struct_size_is_sane() {
        // The kernel writes through a pointer-to-Statfs; on macOS the
        // 64-bit-inode statfs is 2168 bytes per the SDK header. We
        // don't *require* exactly that size (only the prefix matters),
        // but it should comfortably exceed 2 KiB and not exceed 4 KiB.
        let sz = std::mem::size_of::<Statfs>();
        assert!(
            (2048..=4096).contains(&sz),
            "Statfs size {sz} outside expected window"
        );
    }
}
