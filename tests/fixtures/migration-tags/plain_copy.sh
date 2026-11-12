#!/bin/bash
echo "Copy the default waybar config into the user's config"
mkdir -p "$HOME/.config/waybar"
cp "$OMARCHY_PATH/default/waybar/config.jsonc" "$HOME/.config/waybar/config.jsonc"
