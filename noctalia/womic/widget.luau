-- Bar toggle for wo-mic-linux. State comes from `womic-toggle status`.
local state = "off"

local looks = {
  connected = { glyph = "microphone", color = "primary", tip = "WO Mic: connected — click to stop" },
  connecting = { glyph = "microphone", color = "tertiary", tip = "WO Mic: waiting for phone — click to stop" },
  off = { glyph = "microphone-off", color = "on_surface_variant", tip = "WO Mic: off — click to start" },
}

local function render()
  local look = looks[state] or looks.off
  barWidget.setGlyph(look.glyph)
  barWidget.setGlyphColor(look.color)
  barWidget.setTooltip(look.tip)
end

local function refresh()
  noctalia.runAsync({ "womic-toggle", "status" }, function(result)
    local s = string.gsub(result.stdout or "", "%s+", "")
    if looks[s] and s ~= state then
      state = s
      render()
    end
  end)
end

render()
refresh()

function update()
  noctalia.setUpdateInterval(2000)
  refresh()
end

function onClick()
  -- Optimistic feedback; the poll corrects it.
  state = (state == "off") and "connecting" or "off"
  render()
  noctalia.runAsync({ "womic-toggle", "toggle" })
end

function onRightClick()
  noctalia.runInTerminal("tail -f \"${XDG_RUNTIME_DIR:-/tmp}/womic/womic.log\"")
end
