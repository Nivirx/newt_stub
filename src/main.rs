#![no_std]
#![no_main]
#![feature(ptr_internals)]
#![feature(vec_into_raw_parts)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate alloc;

// Keep this line to ensure the `mem*` functions are linked in.
extern crate rlibc;

extern crate goblin;
extern crate uefi;
extern crate uefi_services;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::MaybeUninit;

use arrayvec::ArrayVec;
use uefi::proto::media::file::{File, FileAttribute, FileInfo};
use uefi::proto::media::file::{FileHandle, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, SearchType};
use uefi::table::Runtime;
use uefi::{prelude::*, proto};

const EFI_KERNEL_NAME: &str = "KERNEL";

#[repr(C)]
struct EBootTable {
    sys_table: Option<SystemTable<Runtime>>,
    mmap_buf: Option<*mut u8>,
    mmap_len: Option<usize>,
    mmap_cap: Option<usize>,
}

impl EBootTable {
    pub unsafe fn new() -> *mut EBootTable {
        let value = Box::new(EBootTable {
            sys_table: None,
            mmap_buf: None,
            mmap_len: None,
            mmap_cap: None,
        });
        Box::into_raw(value)
    }

    pub fn update(&mut self, st: SystemTable<Runtime>, mmap_buf: Vec<u8>) {
        let (ptr, len, cap) = mmap_buf.into_raw_parts();
        self.sys_table = Some(st);
        self.mmap_buf = Some(ptr);
        self.mmap_len = Some(len);
        self.mmap_cap = Some(cap);
    }
}

#[no_mangle]
pub extern "win64" fn efi_main(
    efi_image_handle: uefi::Handle,
    mut sys_table: SystemTable<Boot>,
) -> ! {
    // Initialize logging, memory allocation and uefi services
    uefi_services::init(&mut sys_table).expect_success("Failed to init UEFI Utilities!");

    let out = sys_table.stdout();

    out.set_color(
        proto::console::text::Color::Green,
        proto::console::text::Color::Black,
    )
    .expect_success("Failed to set console colors");
    out.clear().expect_success("Failed to clear console");

    // scoped to help clean up after all of this stuff goes out of scope
    {
        // output firmware-vendor (CStr16 to Rust string)
        // max size of 32 characters
        let mut buf = arrayvec::ArrayString::<32>::new();
        sys_table.firmware_vendor().as_str_in_buf(&mut buf).unwrap();
        info!("Firmware Vendor: {}", buf.as_str());
    }

    // more scoping to help keep the scope clean
    {
        let rev = sys_table.uefi_revision();
        let (major, minor) = (rev.major(), rev.minor());
        let buf = format!("UEFI {}.{}", major, minor / 10);
        info!("{}", buf);

        assert!(major >= 2, "Running on an old, unsupported version of UEFI");
        assert!(
            minor >= 30,
            "Old version of UEFI 2, some features might not be available."
        );
    }

    //memory_map(&sys_table.boot_services());
    let kernel_image_handle =
        match get_kernel_image_handle(sys_table.boot_services(), efi_image_handle) {
            Some(t) => t,
            None => panic!("unable to get kernel image file handle"),
        };

    let kernel_entry = load_kernel_image(kernel_image_handle, sys_table.boot_services());
    info!("Using {:#?} as entry point", &kernel_entry);

    // Build a buffer big enough to handle the memory map
    // TODO: this is aligned by chance because of how the uefi-rs allocator works
    // it would be nice to get rid of heap allocations with arrayvec on the stack, but there isn't a good way to
    // "set" the allignment of stuff allocated on the stack.
    let mut mmap_buf = {
        let mmap_size = sys_table.boot_services().memory_map_size();
        let vec_size = mmap_size.map_size + (mmap_size.map_size as f32 * 0.125) as usize;
        create_vec_buf(vec_size)
    };

    // transmute to function pointer from entry point
    let kmain: extern "C" fn(eboot: *mut EBootTable) =
        unsafe { core::mem::transmute(kernel_entry) };
    // allocate memory for eboot table before exiting boot services.
    let eboot = unsafe { EBootTable::new() };

    info!("Exiting UEFI Boot services");
    let rt_table = match sys_table.exit_boot_services(efi_image_handle, &mut mmap_buf) {
        Ok(t) => {
            let (rt, _) = t.log();
            rt
        }
        Err(_) => todo!(),
    };

    // update eboot table with Runtime view of SystemTable and memory map buffer
    unsafe {
        eboot
            .as_mut()
            .expect("error creating eboot table")
            .update(rt_table, mmap_buf)
    };
    // jump to kernel entry point
    (kmain)(eboot);

    panic!();
}

// TODO: this function is getting large, it looks like it might be time to break it up a bit.
fn get_kernel_image_handle(
    bt: &BootServices,
    efi_image_handle: uefi::Handle,
) -> Option<FileHandle> {
    let proto_query = SearchType::from_proto::<SimpleFileSystem>();

    let buf_size = bt
        .locate_handle(proto_query, None)
        .expect("Failed to get required handle buf size")
        .log();

    // allocate enough stack space for 8 handles...might need more on some systems...
    // TODO: check what a good value would be here, since it has to be const
    let mut buf: ArrayVec<MaybeUninit<Handle>, 8> =
        arrayvec::ArrayVec::<MaybeUninit<Handle>, 8>::new();
    unsafe {
        buf.set_len(buf_size);
    }

    let _ = bt
        .locate_handle(proto_query, Some(&mut buf))
        .expect("Failed to get result size for handle buffer")
        .log();

    info!("Found {} valid EFI FileSystem handles", buf.len());

    let params = OpenProtocolParams {
        handle: unsafe { buf[0].assume_init() },
        agent: efi_image_handle,
        controller: None,
    };

    let proto_volume: ScopedProtocol<SimpleFileSystem> =
        match bt.open_protocol(params, OpenProtocolAttributes::GetProtocol) {
            Ok(sp) => sp.log(),
            Err(e) => panic!("{:#?}", e),
        };

    let volume = match unsafe { proto_volume.interface.get().as_mut() } {
        Some(sfs) => sfs,
        None => panic!("no filesystem found"),
    };

    let mut dir = volume
        .open_volume()
        .expect("Unable to open FileSystem volume root dir")
        .log();

    // Must be alligned, so this is left as a heap allocation
    let mut dir_buf = create_vec_buf(128);

    let mut kernel_exists = false;

    loop {
        match dir.read_entry(&mut dir_buf) {
            Ok(file_info) => {
                match file_info.log() {
                    Some(fi) => {
                        info!("found {:?} name: {}", &fi.attribute(), &fi.file_name());

                        if fi.attribute() == FileAttribute::ARCHIVE {
                            let mut temp_name = arrayvec::ArrayString::<64>::new();
                            let _ = &fi.file_name().as_str_in_buf(&mut temp_name);

                            if temp_name.as_str() == EFI_KERNEL_NAME {
                                kernel_exists = true;
                            }
                        }
                    }
                    None => {
                        // No more entries to get, read_entry() returns None
                        break;
                    }
                }
            }
            Err(_size) => todo!(),
        }
    }

    if kernel_exists {
        info!("Found kernel image");
        let kernel_file = dir
            .open(
                EFI_KERNEL_NAME,
                proto::media::file::FileMode::Read,
                FileAttribute::READ_ONLY,
            )
            .expect("Unable to open kernel image for reading")
            .log();

        Some(kernel_file)
    } else {
        warn!("Unable to locate kernel image!");
        None
    }
}

fn load_kernel_image(mut kernel_handle: FileHandle, bs: &BootServices) -> *const () {
    let mut size_buf = create_vec_buf(4096);

    let kernel_size: usize = kernel_handle
        .get_info::<FileInfo>(&mut size_buf)
        .expect("error getting kernel image file info")
        .log()
        .file_size()
        .try_into()
        .unwrap();

    let mut entry_point: usize = 0x0;

    match kernel_handle.into_type() {
        Ok(f) => match f.log() {
            FileType::Regular(mut kern) => {
                let mut kern_buf = create_vec_buf(kernel_size + 1);

                let bytes = kern.read(&mut kern_buf);

                match goblin::elf::Elf::parse(&kern_buf) {
                    Ok(obj) => {
                        info!(
                            "Found ELF binary with an entry point @ 0x{:X}, loaded {} bytes",
                            obj.header.e_entry,
                            bytes.expect("error reading kernel from disk").log()
                        );
                        entry_point = obj
                            .header
                            .e_entry
                            .try_into()
                            .expect("unable to convert to platform native entry point");

                        for ph in obj.program_headers {
                            if ph.p_vaddr == 0x0 && ph.p_paddr == 0x0 {
                                continue;
                            }
                            info!("Found ELF program header >\nELF Offset:\t{:#X}\nLoad address:\t{:#X} & {:#X}\nFile image size:\t{:#X} bytes\nSize in memory:\t{:#X} bytes",
                                            ph.p_offset, ph.p_vaddr, ph.p_paddr, ph.p_filesz, ph.p_memsz
                                        );

                            unsafe {
                                let src = kern_buf.as_slice();
                                let src_ptr = (src.as_ptr() as usize) + (ph.p_offset as usize);
                                info!("Copying program header from {:#X} to {:#X}, count: {:#X} bytes", &src_ptr, ph.p_vaddr, ph.p_filesz);
                                bs.memmove(
                                    ph.p_vaddr as *mut u8,
                                    src_ptr as *const u8,
                                    ph.p_filesz.try_into().expect("convertion failure"),
                                );
                            }
                        }

                        for s in obj.section_headers {
                            let section_name = obj
                                .shdr_strtab
                                .get_at(s.sh_name)
                                .expect("error parsing section name");
                            if section_name.is_empty() {
                                continue;
                            }
                            info!("Found ELF section header {}\t> {:#X} - {:#X}\t({} bytes)\tALIGN: {:#X}\tFLAGS: {:#X}", section_name, s.sh_addr, s.sh_addr + s.sh_size, s.sh_size, s.sh_addralign,s.sh_flags);
                        }
                    }
                    Err(e) => error!("Error parsing ELF: {}", &e),
                }
            }
            FileType::Dir(_) => todo!(),
        },
        Err(_) => todo!(),
    };

    entry_point as *const ()
}

fn create_vec_buf(vec_size: usize) -> Vec<u8> {
    // inform compiler that data is uninit and should not perform optimizations
    let mut data = MaybeUninit::<Vec<u8>>::uninit();

    unsafe {
        // create vector with correct cap & len, assign without drop or realloc to data
        data.write(Vec::with_capacity(vec_size)).set_len(vec_size);

        // init all elements to zero
        let mut data = data.assume_init();
        for e in &mut data {
            *e = 0;
        }

        // return Vec<u8>
        data
    }
}
