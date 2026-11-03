package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"path/filepath"
	"strconv"
	"sync/atomic"
	"time"
)

const protocolVersion = 1
const maxMessage = 256 * 1024

const (
	tileEmpty    uint8 = 0
	tileCold     uint8 = 1
	tileBuilding uint8 = 2
	tileCached   uint8 = 3
	tileLoaded   uint8 = 4
	tileTerOnly  uint8 = 5
)

type ConfigSummary struct {
	SourceLabel string `json:"source_label"`
	DSF         string `json:"dsf"`
	Size        uint32 `json:"size"`
}

type Job struct {
	ID         string  `json:"id"`
	Kind       string  `json:"kind"`
	Generation string  `json:"generation"`
	State      string  `json:"state"`
	Stage      string  `json:"stage"`
	Done       int     `json:"done"`
	Total      int     `json:"total"`
	Error      *string `json:"error"`
}

type Generation struct {
	ID          string `json:"id"`
	Package     string `json:"package"`
	DSF         string `json:"dsf"`
	Assets      int    `json:"assets"`
	Bytes       uint64 `json:"bytes"`
	Size        uint32 `json:"size"`
	SourceLabel string `json:"source_label"`
	Verified    bool   `json:"verified"`
}

type State struct {
	Version         int            `json:"version"`
	Config          *ConfigSummary `json:"config"`
	Job             *Job           `json:"job"`
	Generations     []Generation   `json:"generations"`
	Installed       *string        `json:"installed"`
	StreamConfig    *StreamConfig  `json:"stream_config"`
	StreamEnabled   bool           `json:"stream_enabled"`
	StreamInstalled *string        `json:"stream_installed"`
	Stream          *StreamStatus  `json:"stream"`
}

type StreamConfig struct {
	SourcePackage  string `json:"source_package"`
	MountPoint     string `json:"mount_point"`
	SourceRevision string `json:"source_revision"`
	TileSource     string `json:"tile_source"`
	Size           uint32 `json:"size"`
	BudgetMiB      uint64 `json:"budget_mib"`
}

type StreamStatus struct {
	Mounted               bool        `json:"mounted"`
	MountPoint            string      `json:"mount_point"`
	SourceLabel           string      `json:"source_label"`
	CacheBytes            uint64      `json:"cache_bytes"`
	OtherManagedBytes     uint64      `json:"other_managed_bytes"`
	BudgetBytes           uint64      `json:"budget_bytes"`
	ReservedBytes         uint64      `json:"reserved_bytes"`
	BorrowedLightingBytes uint64      `json:"borrowed_lighting_bytes"`
	CachedAssets          int         `json:"cached_assets"`
	PinnedAssets          int         `json:"pinned_assets"`
	OpenHandles           int         `json:"open_handles"`
	ActiveConsumers       int         `json:"active_consumers"`
	Pending               int         `json:"pending"`
	Working               int         `json:"working"`
	CacheHits             uint64      `json:"cache_hits"`
	ColdBuilds            uint64      `json:"cold_builds"`
	NetworkRequests       uint64      `json:"network_requests"`
	NetworkBytes          uint64      `json:"network_bytes"`
	Coalesced             uint64      `json:"coalesced"`
	Evictions             uint64      `json:"evictions"`
	Failures              uint64      `json:"failures"`
	Reads                 uint64      `json:"reads"`
	ServedBytes           uint64      `json:"served_bytes"`
	DDSHeaderReads        uint64      `json:"dds_header_reads"`
	DDSPayloadReads       uint64      `json:"dds_payload_reads"`
	DDSReadSamples        [][2]uint64 `json:"dds_read_samples"`
	MipReads              []uint64    `json:"mip_reads"`
	TelemetryState        string      `json:"telemetry_state"`
	FlightPhase           string      `json:"flight_phase"`
	AircraftLat           *float64    `json:"aircraft_lat"`
	AircraftLon           *float64    `json:"aircraft_lon"`
	AircraftAGLM          *float64    `json:"aircraft_agl_m"`
	GroundSpeedMPS        *float64    `json:"ground_speed_mps"`
	TrackDeg              *float64    `json:"track_deg"`
	LookaheadQueued       int         `json:"lookahead_queued"`
	LookaheadCompleted    uint64      `json:"lookahead_completed"`
	LookaheadCancelled    uint64      `json:"lookahead_cancelled"`
	PreparedTiles         int         `json:"prepared_tiles"`
	CachedTiles           int         `json:"cached_tiles"`
	BuildingTiles         int         `json:"building_tiles"`
	TerLoadedTiles        int         `json:"ter_loaded_tiles"`
	LoadedTiles           int         `json:"loaded_tiles"`
	TileGridCols          int         `json:"tile_grid_cols"`
	TileGridRows          int         `json:"tile_grid_rows"`
	TileGrid              []uint8     `json:"tile_grid"`
	LastReader            *string     `json:"last_reader"`
	LastError             *string     `json:"last_error"`
}

type wireResponse struct {
	Version int             `json:"version"`
	ID      string          `json:"id"`
	Result  json.RawMessage `json:"result"`
	Error   *string         `json:"error"`
}

type Client struct {
	dataDir string
	nextID  atomic.Uint64
}

func (c *Client) call(method string, params any) (State, json.RawMessage, error) {
	var state State
	conn, err := net.DialTimeout("unix", filepath.Join(c.dataDir, "control.sock"), time.Second)
	if err != nil {
		return state, nil, fmt.Errorf("engine unavailable: %w", err)
	}
	defer conn.Close()
	if err = conn.SetDeadline(time.Now().Add(10 * time.Second)); err != nil {
		return state, nil, err
	}
	id := strconv.FormatUint(c.nextID.Add(1), 10)
	request := struct {
		Version int    `json:"version"`
		ID      string `json:"id"`
		Method  string `json:"method"`
		Params  any    `json:"params"`
	}{protocolVersion, id, method, params}
	bytes, err := json.Marshal(request)
	if err != nil {
		return state, nil, err
	}
	if len(bytes) >= maxMessage {
		return state, nil, errors.New("request exceeds protocol limit")
	}
	if _, err = conn.Write(append(bytes, '\n')); err != nil {
		return state, nil, err
	}
	line, err := bufio.NewReader(io.LimitReader(conn, maxMessage+1)).ReadBytes('\n')
	if err != nil {
		return state, nil, fmt.Errorf("reading engine response: %w", err)
	}
	if len(line) > maxMessage {
		return state, nil, errors.New("engine response exceeds protocol limit")
	}
	var response wireResponse
	if err = json.Unmarshal(line, &response); err != nil {
		return state, nil, fmt.Errorf("invalid engine response: %w", err)
	}
	if response.Version != protocolVersion || response.ID != id {
		return state, nil, errors.New("engine protocol version or request ID mismatch")
	}
	if response.Error != nil {
		return state, nil, errors.New(*response.Error)
	}
	if err = json.Unmarshal(response.Result, &state); err != nil {
		return state, nil, fmt.Errorf("invalid engine state: %w", err)
	}
	if state.Version != protocolVersion {
		return state, nil, errors.New("unsupported engine state version")
	}
	return state, response.Result, nil
}
