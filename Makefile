ARCH ?= x86_64
TARGET ?= $(ARCH)-unknown-uefi

OVMF_FW := OVMF_CODE.fd
OVMF_VARS = OVMF_VARS.fd
BOOT_DIR := BOOT

qemu_args := -nodefaults -vga std -monitor vc:1440x900 -serial stdio -machine q35,accel=kvm:hvf -no-shutdown -no-reboot  -m 256M
qemu_efi := -drive if=pflash,format=raw,readonly=on,file=$(OVMF_FW)
qemu_efi_vars :=  -drive if=pflash,format=raw,file=$(OVMF_VARS)
qemu_drive := -drive format=raw,file=fat:rw:$(BOOT_DIR)

newt_stub_debug := target/$(TARGET)/debug/newt_stub.efi
newt_stub_release := target/$(TARGET)/release/newt_stub.efi

.PHONY: all release debug clean run-debug run

all: $(newt_stub_debug) $(newt_stub_release)
debug: $(newt_stub_debug)
release: $(newt_stub_release)

clean:
	rm -rv $(BOOT_DIR)
	@RUST_TARGET_PATH=$(shell pwd) cargo clean --target $(TARGET)

run-debug: $(newt_stub_debug)
	@RUST_TARGET_PATH=$(shell pwd) cargo +nightly build -Z build-std --target $(TARGET) --verbose
	mkdir -p $(BOOT_DIR)/EFI/BOOT/
	cp -v $(newt_stub_debug) $(BOOT_DIR)/EFI/BOOT/BOOTX64.EFI
	@qemu-system-$(ARCH) $(qemu_args) $(qemu_efi) $(qemu_efi_vars) $(qemu_drive)

run: $(newt_stub_release)
	@RUST_TARGET_PATH=$(shell pwd) cargo +nightly build -Z build-std --target $(TARGET) --release
	mkdir -p $(BOOT_DIR)/EFI/BOOT/
	cp -v $(newt_stub_release) $(BOOT_DIR)/EFI/BOOT/BOOTX64.EFI
	@qemu-system-$(ARCH) $(qemu_args) $(qemu_efi) $(qemu_efi_vars) $(qemu_drive)

$(newt_stub_debug):
	@RUST_TARGET_PATH=$(shell pwd) cargo +nightly build -Z build-std --target $(TARGET) --verbose
	mkdir -p $(BOOT_DIR)/EFI/BOOT/
	cp -v $(newt_stub_debug) $(BOOT_DIR)/EFI/BOOT/BOOTX64.EFI

$(newt_stub_release):
	@RUST_TARGET_PATH=$(shell pwd) cargo +nightly build -Z build-std --target $(TARGET) --release
	mkdir -p $(BOOT_DIR)/EFI/BOOT/
	cp -v $(newt_stub_release) $(BOOT_DIR)/EFI/BOOT/BOOTX64.EFI
