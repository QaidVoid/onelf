# Build bootstrap payloads as flat binary blobs.
# Outputs are checked into the repo and embedded via include_bytes!.

CC_X86_64  ?= gcc
AS_X86_64  ?= as
LD_X86_64  ?= ld
CC_AARCH64 ?= aarch64-unknown-linux-gnu-gcc
AS_AARCH64 ?= aarch64-unknown-linux-gnu-as
LD_AARCH64 ?= aarch64-unknown-linux-gnu-ld
OBJCOPY_AARCH64 ?= aarch64-unknown-linux-gnu-objcopy

CFLAGS = -nostdlib -static -fPIC -ffreestanding -O2 -fno-stack-protector \
         -fno-asynchronous-unwind-tables -Wall -Wextra

# Flags for the onelf-env constructor shared objects. -nostdlib keeps
# the .so free of any DT_NEEDED; `setenv`/`dlopen` stay UNDEF and resolve
# from the application's own libc at load time, so one per-arch blob
# works for both glibc and musl.
SOFLAGS = -nostdlib -shared -fPIC -ffreestanding -O2 -fno-stack-protector \
          -fno-asynchronous-unwind-tables -fno-builtin \
          -fno-tree-loop-distribute-patterns -Wall -Wextra

all: bootstrap_x86_64.bin bootstrap_aarch64.bin \
     onelf_env_x86_64.so onelf_env_aarch64.so

bootstrap_x86_64.bin: trampoline_x86_64.o bootstrap_x86_64.o
	$(LD_X86_64) -nostdlib --oformat binary -e _onelf_start \
		-T payload.ld $^ -o $@
	@echo "x86_64 payload: $$(wc -c < $@) bytes"

trampoline_x86_64.o: trampoline_x86_64.S
	$(AS_X86_64) --64 -o $@ $<

bootstrap_x86_64.o: bootstrap_x86_64.c
	$(CC_X86_64) $(CFLAGS) -c -o $@ $<

bootstrap_aarch64.elf: trampoline_aarch64.o bootstrap_aarch64.o
	$(LD_AARCH64) -nostdlib -e _onelf_start \
		-T payload.ld $^ -o $@

bootstrap_aarch64.bin: bootstrap_aarch64.elf
	$(OBJCOPY_AARCH64) -O binary $< $@
	@echo "aarch64 payload: $$(wc -c < $@) bytes"

trampoline_aarch64.o: trampoline_aarch64.S
	$(AS_AARCH64) -o $@ $<

bootstrap_aarch64.o: bootstrap_aarch64.c
	$(CC_AARCH64) $(CFLAGS) -c -o $@ $<

onelf_env_x86_64.so: onelf_env.c
	$(CC_X86_64) $(SOFLAGS) -Wl,-soname,libonelf-env.so -o $@ $<
	@echo "x86_64 onelf-env: $$(wc -c < $@) bytes"

onelf_env_aarch64.so: onelf_env.c
	$(CC_AARCH64) $(SOFLAGS) -Wl,-soname,libonelf-env.so -o $@ $<
	@echo "aarch64 onelf-env: $$(wc -c < $@) bytes"

clean:
	rm -f *.o *.elf

.PHONY: all clean
