#!/bin/bash

cd target/img

rm -f efi.img
dd if=/dev/zero of=efi.img bs=1k count=5760
mkfs.vfat efi.img
sudo mount efi.img mnt
sudo mkdir -p mnt/EFI/BOOT
sudo mkdir -p mnt/EFI/Oceanic
sudo cp ../BootX64.efi mnt/EFI/BOOT
sudo cp ../H2O.k mnt/EFI/Oceanic
sudo umount mnt

qemu-img convert efi.img -f raw -O vmdk efi.vmdk
cp efi.vmdk efi.vbox.vmdk