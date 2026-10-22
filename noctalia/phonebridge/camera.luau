-- Bar toggle for the phonebridge webcam (a separate camera-only instance, so it
-- works with or without the microphone). State comes from `phonebridge camera status`.
--   left click:   start / stop the camera
--   right click:  preview the webcam (mpv, ffplay or vlc) while streaming
--   middle click: follow the camera log in a terminal
local state = "off"

local looks = {
  streaming = { glyph = "video", color = "primary", tip = "Phonebridge camera: streaming — click to stop, right-click to preview" },
  connecting = { glyph = "video", color = "tertiary", tip = "Phonebridge camera: waiting for phone — click to stop" },
  off = { glyph = "video-off", color = "on_surface_variant", tip = "Phonebridge camera: off — click to start" },
}

-- First available viewer on the Phonebridge webcam device, detached from Noctalia.
local preview = [[
dev=$(phonebridge camera device) || exit 1
if command -v mpv >/dev/null; then
  set -- mpv --profile=low-latency --untimed --title="Phonebridge camera" "av://v4l2:$dev"
elif command -v ffplay >/dev/null; then
  set -- ffplay -loglevel error -fflags nobuffer -window_title "Phonebridge camera" -f v4l2 "$dev"
elif command -v vlc >/dev/null; then
  set -- vlc --no-video-title-show "v4l2://$dev"
else
  exec notify-send -a Phonebridge "Phonebridge camera" "No viewer found (install mpv, ffplay or vlc)"
fi
exec setsid -f "$@" >/dev/null 2>&1 </dev/null
]]

local function render()
  local look = looks[state] or looks.off
  barWidget.setGlyph(look.glyph)
  barWidget.setGlyphColor(look.color)
  barWidget.setTooltip(look.tip)
end

local function refresh()
  noctalia.runAsync({ "phonebridge", "camera", "status" }, function(result)
    local s = string.gsub(result.stdout or "", "%s+", "")
    if looks[s] and s ~= state then
      state = s
      render()
    end
  end)
end

local function run(cmd, optimistic)
  -- Optimistic feedback; the next status poll corrects it.
  state = optimistic
  render()
  if cmd == "start" then
    -- Detached: start blocks until the handshake finishes.
    noctalia.runAsync({ "phonebridge", "camera", cmd })
  else
    noctalia.runAsync({ "phonebridge", "camera", cmd }, function()
      refresh()
    end)
  end
end

render()
refresh()

function update()
  noctalia.setUpdateInterval(1000)
  refresh()
end

function onClick()
  if state == "off" then
    run("start", "connecting")
  else
    run("stop", "off")
  end
end

function onRightClick()
  if state == "streaming" then
    noctalia.runAsync({ "sh", "-c", preview })
  elseif state == "off" then
    run("start", "connecting")
  end
end

function onMiddleClick()
  noctalia.runInTerminal("tail -f \"${XDG_RUNTIME_DIR:-/tmp}/phonebridge/camera.log\"")
end
