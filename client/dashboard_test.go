package main

import (
	"errors"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
)

func TestDashboardBounds(t *testing.T) {
	for _, viewport := range [][2]int{{40, 12}, {80, 24}, {120, 40}} {
		m := newDashboard(nil)
		m.width, m.height = viewport[0], viewport[1]
		m.loaded = true
		m.state.Config = &ConfigSummary{SourceLabel: "Raleigh fixture", Size: 1024}
		m.state.Job = &Job{Kind: "prepare", State: "running", Stage: "compiling", Done: 15, Total: 50}
		view := m.View()
		if !view.AltScreen {
			t.Fatal("dashboard did not request alternate screen")
		}
		if lipgloss.Width(view.Content) > m.width || lipgloss.Height(view.Content) > m.height {
			t.Fatalf("rendered %dx%d in %dx%d viewport", lipgloss.Width(view.Content),
				lipgloss.Height(view.Content), m.width, m.height)
		}
		if !strings.Contains(view.Content, "GROUNDEFFECT") {
			t.Fatal("missing title")
		}
	}
}

func TestPreparationConfirmation(t *testing.T) {
	m := newDashboard(nil)
	model, command := m.Update(tea.KeyPressMsg{Code: 'p'})
	m = model.(dashboard)
	if !m.confirm || command != nil {
		t.Fatal("preparation was not gated by confirmation")
	}
	model, command = m.Update(tea.KeyPressMsg{Code: tea.KeyEscape})
	if model.(dashboard).confirm || command != nil {
		t.Fatal("escape did not dismiss confirmation without engine work")
	}
}

func TestConnectionStatusAndRecovery(t *testing.T) {
	m := newDashboard(nil)
	if !strings.Contains(m.View().Content, "ENGINE CONNECTING") {
		t.Fatal("initial connection state is not visible")
	}
	for _, action := range []bool{false, true} {
		model, _ := m.Update(resultMsg{state: State{Version: protocolVersion}})
		m = model.(dashboard)
		if !strings.Contains(m.View().Content, "ENGINE CONNECTED") {
			t.Fatal("successful engine response is not shown as connected")
		}
		model, _ = m.Update(resultMsg{err: errors.New("engine unavailable")})
		m = model.(dashboard)
		view := m.View().Content
		if !strings.Contains(view, "ENGINE DISCONNECTED") || strings.Contains(view, "ENGINE CONNECTED") ||
			!strings.Contains(view, "DISPLAYED STATE MAY BE STALE") {
			t.Fatal("connection failure hides the disconnected or stale state")
		}
		model, _ = m.Update(resultMsg{state: State{Version: protocolVersion}, action: action})
		m = model.(dashboard)
		if !strings.Contains(m.View().Content, "ENGINE CONNECTED") || m.connectionErr != nil {
			t.Fatal("successful status or action response did not restore connection state")
		}
	}
}

func TestRecordedInstallationVisibility(t *testing.T) {
	m := newDashboard(nil)
	m.loaded, m.width, m.height = true, 120, 40
	if !strings.Contains(m.View().Content, "INSTALL: NONE") {
		t.Fatal("absent installation record is not shown")
	}

	installed := "/scenery/z_groundeffect -> published-id"
	m.state.Installed = &installed
	m.state.Generations = []Generation{{ID: "published-id", Package: "/data/scenery", Verified: true}}
	m.selected = 0
	for page := 0; page < 3; page++ {
		m.page = page
		if !strings.Contains(m.View().Content, "INSTALL RECORDED") {
			t.Fatal("recorded installation is not visible across dashboard pages")
		}
	}
	m.page = 1
	if !strings.Contains(m.View().Content, installed) ||
		!strings.Contains(m.View().Content, "no live X-Plane connection") {
		t.Fatal("installation details or simulator-connection distinction is missing")
	}
}

func TestAutomaticReaderIsNotPresentedAsSimulatorConsumption(t *testing.T) {
	m := newDashboard(nil)
	m.loaded = true
	m.state.StreamConfig = &StreamConfig{Size: 1024}
	m.state.Stream = &StreamStatus{Mounted: true, BudgetBytes: 8 * 1024 * 1024 * 1024}
	view := m.View().Content
	if !strings.Contains(view, "READER MOUNTED") || !strings.Contains(view, "No file reader observed yet.") ||
		strings.Contains(view, "PREPARATION") {
		t.Fatal("mount health was confused with real consumption or offline preparation")
	}
	reader := "X-Plane-x86_64 PID 123 / terrain/test.ter"
	m.state.Stream.Reads, m.state.Stream.ActiveConsumers = 12, 1
	m.state.Stream.LastReader = &reader
	if !strings.Contains(m.View().Content, "12 kernel reads") || !strings.Contains(m.View().Content, "Observed: X-Plane") {
		t.Fatal("actual reader activity is not visible")
	}
	model, cmd := m.Update(tea.KeyPressMsg{Code: 's'})
	if !model.(dashboard).confirmStream || cmd != nil {
		t.Fatal("stop needs confirmation")
	}
	model, cmd = model.Update(tea.KeyPressMsg{Code: tea.KeyEscape})
	if model.(dashboard).confirmStream || cmd != nil {
		t.Fatal("cancel dispatched stop")
	}
	m.state.Stream.Mounted = false
	failed := "mount disconnected"
	m.state.Stream.LastError = &failed
	if !strings.Contains(m.View().Content, "READER STOPPED") || !strings.Contains(m.View().Content, failed) {
		t.Fatal("disconnection is hidden")
	}
}

func TestStreamTileGridAndLoadProgress(t *testing.T) {
	grid := make([]uint8, 16*16)
	grid[0] = 4
	grid[1] = 5
	grid[2] = 2
	grid[3] = 3
	grid[4] = 1
	for _, viewport := range [][2]int{{80, 24}, {120, 40}} {
		m := newDashboard(nil)
		m.loaded, m.width, m.height = true, viewport[0], viewport[1]
		m.state.StreamConfig = &StreamConfig{Size: 1024}
		lat, lon := 35.8776, -78.7875
		gsMPS := 142.0 / 1.94384
		trk := 45.0
		m.state.Stream = &StreamStatus{
			Mounted:        true,
			BudgetBytes:    8 * 1024 * 1024 * 1024,
			TelemetryState: "fresh",
			FlightPhase:    "cruise",
			AircraftLat:    &lat,
			AircraftLon:    &lon,
			GroundSpeedMPS: &gsMPS,
			TrackDeg:       &trk,
			PreparedTiles:  3176,
			TerLoadedTiles: 3176,
			LoadedTiles:    1588,
			CachedTiles:    3176,
			TileGridCols:   16,
			TileGridRows:   16,
			TileGrid:       grid,
		}
		view := m.View().Content
		if lipgloss.Width(view) > m.width || lipgloss.Height(view) > m.height {
			t.Fatalf("rendered %dx%d in %dx%d viewport", lipgloss.Width(view), lipgloss.Height(view), m.width, m.height)
		}
		if !strings.Contains(view, "LOAD") || !strings.Contains(view, "GRID") || !strings.Contains(view, "■") ||
			!strings.Contains(view, "142kt") || !strings.Contains(view, "↗") {
			t.Fatalf("missing tile load progress, box grid, or center aircraft badge in %dx%d viewport", m.width, m.height)
		}
	}
}
