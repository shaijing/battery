use anyhow::{Context, Result, bail};
use std::ffi::{c_char, c_void};

const KERNEL_INDEX_SMC: u32 = 2;
const SMC_CMD_READ_BYTES: u8 = 5;
const SMC_CMD_WRITE_BYTES: u8 = 6;
const SMC_CMD_READ_KEYINFO: u8 = 9;
const SMC_MAX_BYTES: usize = 32;

type KernReturn = i32;
type MachPort = u32;
type IoConnect = u32;
type IoObject = u32;
type IoIterator = u32;

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: i8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Version {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PowerLimitData {
    version: u16,
    length: u16,
    cpu_limit: u32,
    gpu_limit: u32,
    memory_limit: u32,
}

#[repr(C)]
struct KeyData {
    key: u32,
    version: Version,
    power_limit: PowerLimitData,
    key_info: KeyInfo,
    result: i8,
    status: i8,
    command: u8,
    data32: u32,
    bytes: [u8; SMC_MAX_BYTES],
}

const _: () = assert!(std::mem::size_of::<KeyData>() == 80);

impl KeyData {
    fn zeroed() -> Self {
        // The SMC ABI expects every unused input field to be zero.
        unsafe { std::mem::zeroed() }
    }
}

#[derive(Debug)]
pub(crate) struct Reading {
    pub(crate) data_type: String,
    pub(crate) bytes: Vec<u8>,
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOMasterPort(bootstrap_port: MachPort, master_port: *mut MachPort) -> KernReturn;
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(
        master_port: MachPort,
        matching: *mut c_void,
        existing: *mut IoIterator,
    ) -> KernReturn;
    fn IOIteratorNext(iterator: IoIterator) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
    fn IOServiceOpen(
        service: IoObject,
        owning_task: MachPort,
        service_type: u32,
        connect: *mut IoConnect,
    ) -> KernReturn;
    fn IOServiceClose(connect: IoConnect) -> KernReturn;
    fn IOConnectCallStructMethod(
        connect: IoConnect,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> KernReturn;
}

unsafe extern "C" {
    static mach_task_self_: MachPort;
}

struct Connection(IoConnect);

impl Connection {
    fn open() -> Result<Self> {
        let mut master_port = 0;
        let result = unsafe { IOMasterPort(0, &mut master_port) };
        check(result, "IOMasterPort")?;

        let matching = unsafe { IOServiceMatching(c"AppleSMC".as_ptr()) };
        if matching.is_null() {
            bail!("IOServiceMatching(AppleSMC) returned null");
        }
        let mut iterator = 0;
        let result = unsafe { IOServiceGetMatchingServices(master_port, matching, &mut iterator) };
        check(result, "IOServiceGetMatchingServices")?;

        let service = unsafe { IOIteratorNext(iterator) };
        unsafe {
            IOObjectRelease(iterator);
        }
        if service == 0 {
            bail!("AppleSMC service was not found");
        }

        let mut connect = 0;
        let result = unsafe { IOServiceOpen(service, mach_task_self_, 0, &mut connect) };
        unsafe {
            IOObjectRelease(service);
        }
        check(result, "IOServiceOpen(AppleSMC)")?;
        Ok(Self(connect))
    }

    fn call(&self, input: &KeyData) -> Result<KeyData> {
        let mut output = KeyData::zeroed();
        let mut output_size = std::mem::size_of::<KeyData>();
        let result = unsafe {
            IOConnectCallStructMethod(
                self.0,
                KERNEL_INDEX_SMC,
                (input as *const KeyData).cast(),
                std::mem::size_of::<KeyData>(),
                (&mut output as *mut KeyData).cast(),
                &mut output_size,
            )
        };
        check(result, "IOConnectCallStructMethod(AppleSMC)")?;
        Ok(output)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.0);
        }
    }
}

pub(crate) fn read(key: &str) -> Result<Reading> {
    let connection = Connection::open()?;
    read_with_connection(&connection, key)
}

pub(crate) fn write(key: &str, bytes: &[u8]) -> Result<()> {
    let connection = Connection::open()?;
    let current = read_with_connection(&connection, key)?;
    if current.bytes.len() != bytes.len() {
        bail!(
            "SMC key {key} expects {} bytes, got {}",
            current.bytes.len(),
            bytes.len()
        );
    }
    if bytes.len() > SMC_MAX_BYTES {
        bail!("SMC write is too large: {} bytes", bytes.len());
    }

    let mut input = KeyData::zeroed();
    input.key = encode_key(key)?;
    input.command = SMC_CMD_WRITE_BYTES;
    input.key_info.data_size = bytes.len() as u32;
    input.bytes[..bytes.len()].copy_from_slice(bytes);
    connection.call(&input).context("SMC write failed")?;
    Ok(())
}

fn read_with_connection(connection: &Connection, key: &str) -> Result<Reading> {
    let encoded_key = encode_key(key)?;
    let mut input = KeyData::zeroed();
    input.key = encoded_key;
    input.command = SMC_CMD_READ_KEYINFO;
    let info = connection.call(&input)?.key_info;
    let size = usize::try_from(info.data_size).context("invalid SMC key data size")?;
    if size == 0 || size > SMC_MAX_BYTES {
        bail!("SMC key {key} has unsupported size {size}");
    }

    let mut input = KeyData::zeroed();
    input.key = encoded_key;
    input.command = SMC_CMD_READ_BYTES;
    input.key_info.data_size = info.data_size;
    let value = connection.call(&input)?;
    let data_type = info.data_type.to_be_bytes();
    let data_type = String::from_utf8_lossy(&data_type).trim().to_owned();
    Ok(Reading {
        data_type,
        bytes: value.bytes[..size].to_vec(),
    })
}

fn encode_key(key: &str) -> Result<u32> {
    let bytes: [u8; 4] = key
        .as_bytes()
        .try_into()
        .with_context(|| format!("SMC keys must be exactly four bytes: {key:?}"))?;
    Ok(u32::from_be_bytes(bytes))
}

fn check(result: KernReturn, operation: &str) -> Result<()> {
    if result != 0 {
        bail!(
            "{operation} failed with IOKit status 0x{:08x}",
            result as u32
        );
    }
    Ok(())
}
