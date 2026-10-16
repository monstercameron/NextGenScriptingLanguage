hl.config({
    general = {
        gaps_in = 4,
        gaps_out = 8,
        border_size = 1,
        layout = "dwindle",
    },
    decoration = {
        blur = { enabled = false },
        shadow = { enabled = false },
    },
    animations = {
        enabled = false,
    },
    misc = {
        disable_hyprland_logo = true,
    },
})

hl.monitor({
    output = "",
    mode = "preferred",
    position = "auto",
    scale = 1,
})

hl.on("hyprland.start", function()
    hl.exec_cmd("/home/arch/execute/tools/desktop_ci/guest-session.sh")
end)
