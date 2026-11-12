#!/bin/bash
echo "Gate fingerprint sign-in on the lid"
# Keep this in step with the sudo and polkit stacks.
if ! grep -q pam_lid /etc/pam.d/sudo; then
  sudo sed -i '0,/pam_fprintd/s//pam_lid.so\n&/' /etc/pam.d/sudo
fi
sudo sed -i '0,/pam_fprintd/s//pam_lid.so\n&/' /etc/pam.d/polkit-1
