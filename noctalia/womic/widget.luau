-- Bar toggle for wo-mic-linux. State comes from `womic status`.
--   left click:   start when off, otherwise mute / unmute
--   right click:  start / stop
--   middle click: follow the log in a terminal
local state = "off"

local looks = {
  connected = { glyph = "microphone", color = "primary", tip = "WO Mic: live — click to mute, right-click to stop" },
  muted = { glyph = "microphone-mute", color = "error", tip = "WO Mic: muted — click to unmute, right-click to stop" },
  connecting = { glyph = "microphone", color = "tertiary", tip = "WO Mic: waiting for phone — right-click to stop" },
  off = { glyph = "microphone-off", color = "on_surface_variant", tip = "WO Mic: off — click to start" },
}

local function render()
  local look = looks[state] or looks.off
  barWidget.setGlyph(look.glyph)
  barWidget.setGlyphColor(look.color)
  barWidget.setTooltip(look.tip)
end

local function refresh()
  noctalia.runAsync({ "womic", "status" }, function(result)
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
    noctalia.runAsync({ "womic", cmd })
  else
    noctalia.runAsync({ "womic", cmd }, function()
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
  elseif state == "muted" then
    run("mute", "connected")
  elseif state == "connected" then
    run("mute", "muted")
  end
end

function onRightClick()
  if state == "off" then
    run("start", "connecting")
  else
    run("stop", "off")
  end
end

function onMiddleClick()
  noctalia.runInTerminal("tail -f \"${XDG_RUNTIME_DIR:-/tmp}/womic/womic.log\"")
end
