#!/bin/bash
echo "Rebuild the initramfs after the vconsole keymap change"
# Without the keymap in the initramfs, a LUKS passphrase prompt can lock the user out.
sudo tee /etc/vconsole.conf >/dev/null <<EOF
KEYMAP=us
EOF
sudo mkinitcpio -P
