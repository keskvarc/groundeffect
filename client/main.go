package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"
	"unicode"

	tea "charm.land/bubbletea/v2"
	"github.com/charmbracelet/x/term"
)

const usage = `GroundEffect 0.1 alpha -- demand-driven preview and native exports

Usage: groundeffect [--data-dir PATH] [--json] COMMAND

  tui                         Open terminal dashboard (default)
  engine start                Start/reconnect to independent engine
  engine stop                 Stop idle engine; prepared scenery stays available
  status                      Show real engine state
  stream configure --config FILE  Save one-time automatic-reader inputs
  stream source --provider NAME [--size N]  Switch tile source (bing|arcgis|clarity|usgs)
  stream start                Mount/reconnect reader; no regional preparation
  stream stop                 Unmount only when readers have exited
  stream install --target ABSOLUTE_PATH  Create a new scenery symlink
  configure --config FILE     Validate mesh and save preparation inputs
  prepare [--wait]             Compile complete generation offline
  verify --generation ID [--wait]
  install --generation ID --target ABSOLUTE_PATH [--wait]

Installation creates a new package symlink only. It never replaces existing
scenery or edits scenery_packs.ini. Simulator qualification is a separate step.
Closing the terminal never stops the reader. Configured operation autostarts
with the engine. Cold textures may wait on network/build work.
`

func defaultDataDir() (string, error) {
	base := os.Getenv("XDG_DATA_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		base = filepath.Join(home, ".local", "share")
	}
	if !filepath.IsAbs(base) {
		return "", errors.New("XDG_DATA_HOME must be absolute")
	}
	return filepath.Join(base, "groundeffect"), nil
}

func readConfig(path string) (json.RawMessage, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	bytes, err := io.ReadAll(io.LimitReader(file, 65537))
	if err != nil {
		return nil, err
	}
	if len(bytes) > 65536 || !json.Valid(bytes) {
		return nil, errors.New("configuration must be valid JSON <=64 KiB")
	}
	return json.RawMessage(bytes), nil
}

func startEngine(client *Client) error {
	if _, _, err := client.call("status", struct{}{}); err == nil {
		return nil
	}
	if err := os.MkdirAll(client.dataDir, 0700); err != nil {
		return err
	}
	executable, err := os.Executable()
	if err != nil {
		return err
	}
	executable, err = filepath.EvalSymlinks(executable)
	if err != nil {
		return err
	}
	engine := filepath.Join(filepath.Dir(executable), "groundeffect-engine")
	logPath := filepath.Join(client.dataDir, "engine.log")
	log, err := os.OpenFile(logPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0600)
	if err != nil {
		return err
	}
	defer log.Close()
	cmd := exec.Command(engine, "serve", "--data-dir", client.dataDir)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	cmd.Stdout, cmd.Stderr = log, log
	if err = cmd.Start(); err != nil {
		return fmt.Errorf("starting sibling engine %s: %w", engine, err)
	}
	exited := make(chan error, 1)
	go func() { exited <- cmd.Wait() }()
	for range 50 {
		if _, _, err = client.call("status", struct{}{}); err == nil {
			return nil
		}
		select {
		case err = <-exited:
			return fmt.Errorf("engine exited (%v); inspect %s", err, logPath)
		case <-time.After(100 * time.Millisecond):
		}
	}
	return fmt.Errorf("engine did not become responsive; inspect %s", logPath)
}

func safeText(s string) string {
	return strings.Map(func(r rune) rune {
		if unicode.IsControl(r) || unicode.In(r, unicode.Cf) {
			return ' '
		}
		return r
	}, s)
}

func printState(state State) {
	fmt.Println("GROUNDEFFECT / Linux alpha")
	if stream := state.Stream; stream != nil {
		fmt.Printf("Reader: mounted=%t / desired=%t / %s\n", stream.Mounted, state.StreamEnabled, safeText(stream.MountPoint))
		fmt.Printf("Streaming source: %s\n", safeText(stream.SourceLabel))
		fmt.Printf("Cache: %.1f MiB / %.0f MiB ceiling / %d assets / %d pinned\n",
			float64(stream.CacheBytes)/(1024*1024), float64(stream.BudgetBytes)/(1024*1024), stream.CachedAssets, stream.PinnedAssets)
		fmt.Printf("Delivery: %d kernel reads / %.1f MiB served / %d cold builds / %d cache hits\n",
			stream.Reads, float64(stream.ServedBytes)/(1024*1024), stream.ColdBuilds, stream.CacheHits)
		fmt.Printf("DDS requests: %d header-only / %d payload\n", stream.DDSHeaderReads, stream.DDSPayloadReads)
		fmt.Printf("Work: %d pending / %d workers / %d handles / %d live reader processes\n",
			stream.Pending, stream.Working, stream.OpenHandles, stream.ActiveConsumers)
		fmt.Printf("Look-ahead: %d queued / %d pre-warmed / %d cancelled\n",
			stream.LookaheadQueued, stream.LookaheadCompleted, stream.LookaheadCancelled)
		if stream.PreparedTiles > 0 {
			fmt.Printf("Region load: %d/%d .ter indexed / %d/%d .dds loaded / %d/%d cached (%d building)\n",
				stream.TerLoadedTiles, stream.PreparedTiles,
				stream.LoadedTiles, stream.PreparedTiles,
				stream.CachedTiles, stream.PreparedTiles,
				stream.BuildingTiles)
		}
		if stream.TelemetryState != "" {
			fmt.Printf("Telemetry: %s (%s)\n", safeText(stream.TelemetryState), safeText(stream.FlightPhase))
		}
		if stream.LastReader != nil {
			fmt.Printf("Observed: %s\n", safeText(*stream.LastReader))
		}
		if stream.LastError != nil {
			fmt.Printf("Reader error: %s\n", safeText(*stream.LastError))
		}
	}
	if state.StreamInstalled != nil {
		fmt.Printf("Streaming scenery: %s\n", safeText(*state.StreamInstalled))
	}
	if state.Config == nil {
		fmt.Println("Native export: not configured")
	} else {
		fmt.Printf("Native export source: %s\nTile: %s / %d px\n", safeText(state.Config.SourceLabel), safeText(state.Config.DSF), state.Config.Size)
	}
	if job := state.Job; job != nil {
		fmt.Printf("Job: %s / %s / %s (%d/%d)\n", safeText(job.ID), safeText(job.Kind), safeText(job.State), job.Done, job.Total)
		if job.Error != nil {
			fmt.Printf("Error: %s\n", safeText(*job.Error))
		}
	}
	for _, generation := range state.Generations {
		fmt.Printf("Generation: %s / verified=%t / %d assets / %.1f MiB logical\n  %s\n",
			safeText(generation.ID), generation.Verified, generation.Assets, float64(generation.Bytes)/(1024*1024), safeText(generation.Package))
	}
	if state.Installed != nil {
		fmt.Printf("Installed: %s\n", safeText(*state.Installed))
	}
	fmt.Println("X-Plane appearance/load time: not qualified by asset verification")
}

func run(args []string) error {
	dataDir, err := defaultDataDir()
	if err != nil {
		return err
	}
	flags := flag.NewFlagSet("groundeffect", flag.ContinueOnError)
	flags.SetOutput(os.Stderr)
	flags.Usage = func() { fmt.Fprint(os.Stderr, usage) }
	flags.StringVar(&dataDir, "data-dir", dataDir, "private engine data directory")
	asJSON := flags.Bool("json", false, "emit machine-readable state")
	if err = flags.Parse(args); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return nil
		}
		return err
	}
	if !filepath.IsAbs(dataDir) {
		dataDir, err = filepath.Abs(dataDir)
		if err != nil {
			return err
		}
	}
	client := &Client{dataDir: dataDir}
	command := flags.Args()
	if len(command) == 0 {
		command = []string{"tui"}
	}
	method := command[0]
	if method == "tui" {
		if len(command) != 1 || *asJSON {
			return errors.New("tui accepts no arguments or --json")
		}
		if !term.IsTerminal(os.Stdin.Fd()) || !term.IsTerminal(os.Stdout.Fd()) {
			return errors.New("tui requires a terminal; use --json status for scripting")
		}
		if err = startEngine(client); err != nil {
			return err
		}
		_, err = tea.NewProgram(newDashboard(client)).Run()
		return err
	}
	params := any(struct{}{})
	wait := false
	switch method {
	case "engine":
		if len(command) != 2 {
			return errors.New("usage: engine start|stop")
		}
		switch command[1] {
		case "start":
			if err = startEngine(client); err != nil {
				return err
			}
			method = "status"
		case "stop":
			method = "shutdown"
		default:
			return errors.New("usage: engine start|stop")
		}
	case "status":
		if len(command) != 1 {
			return errors.New("status accepts no arguments")
		}
	case "stream":
		if len(command) < 2 {
			return errors.New("usage: stream configure|source|start|stop|install")
		}
		method = "stream_" + command[1]
		sub := flag.NewFlagSet("stream "+command[1], flag.ContinueOnError)
		var config, target, provider string
		var size uint
		switch command[1] {
		case "configure":
			sub.StringVar(&config, "config", "", "one-time reader JSON file")
		case "source":
			sub.StringVar(&provider, "provider", "", "tile source (bing|arcgis_world_imagery|arcgis_clarity|usgs_imagery_only)")
			sub.UintVar(&size, "size", 0, "optional DDS size (256|512|1024|2048)")
		case "install":
			sub.StringVar(&target, "target", "", "new absolute scenery package symlink")
		case "start", "stop":
		default:
			return errors.New("usage: stream configure|source|start|stop|install")
		}
		if err = sub.Parse(command[2:]); err != nil {
			return err
		}
		if command[1] == "source" && provider == "" && len(sub.Args()) == 1 {
			provider = sub.Args()[0]
		} else if len(sub.Args()) != 0 {
			return errors.New("unexpected positional arguments")
		}
		switch command[1] {
		case "configure":
			if config == "" {
				return errors.New("stream configure requires --config FILE")
			}
			params, err = readConfig(config)
			if err != nil {
				return err
			}
		case "source":
			if provider == "" {
				return errors.New("stream source requires --provider NAME (bing|arcgis|clarity|usgs)")
			}
			req := map[string]any{"tile_source": provider}
			if size > 0 {
				req["size"] = uint32(size)
			}
			params = req
		case "install":
			if target == "" {
				return errors.New("stream install requires --target ABSOLUTE_PATH")
			}
			params = map[string]string{"target": target}
		}
		if err = startEngine(client); err != nil {
			return err
		}
	case "configure", "prepare", "verify", "install":
		sub := flag.NewFlagSet(method, flag.ContinueOnError)
		var config, generation, target string
		if method == "configure" {
			sub.StringVar(&config, "config", "", "preparation JSON file")
		}
		if method == "verify" || method == "install" {
			sub.StringVar(&generation, "generation", "", "published generation ID")
		}
		if method == "install" {
			sub.StringVar(&target, "target", "", "new absolute scenery package symlink")
		}
		if method != "configure" {
			sub.BoolVar(&wait, "wait", false, "wait for the independent engine job")
		}
		if err = sub.Parse(command[1:]); err != nil {
			return err
		}
		if len(sub.Args()) != 0 {
			return errors.New("unexpected positional arguments")
		}
		switch method {
		case "configure":
			if config == "" {
				return errors.New("configure requires --config FILE")
			}
			params, err = readConfig(config)
			if err != nil {
				return err
			}
		case "verify":
			if generation == "" {
				return errors.New("verify requires --generation ID")
			}
			params = map[string]string{"generation": generation}
		case "install":
			if generation == "" || target == "" {
				return errors.New("install requires --generation ID and --target PATH")
			}
			params = map[string]string{"generation": generation, "target": target}
		}
	default:
		return fmt.Errorf("unknown command %q; use --help", method)
	}
	state, raw, err := client.call(method, params)
	if err != nil {
		return err
	}
	var waitedID string
	if wait && state.Job != nil {
		waitedID = state.Job.ID
	}
	for wait && state.Job != nil && state.Job.ID == waitedID && state.Job.State == "running" {
		time.Sleep(500 * time.Millisecond)
		state, raw, err = client.call("status", struct{}{})
		if err != nil {
			return err
		}
	}
	if *asJSON {
		_, err = os.Stdout.Write(append(raw, '\n'))
	} else {
		printState(state)
	}
	if err != nil {
		return err
	}
	if wait && (state.Job == nil || state.Job.ID != waitedID) {
		return errors.New("waited job was superseded; inspect engine state")
	}
	if wait && state.Job.State != "succeeded" {
		return fmt.Errorf("job %s failed; inspect reported job error", waitedID)
	}
	return nil
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintf(os.Stderr, "groundeffect: %s\n", safeText(err.Error()))
		os.Exit(1)
	}
}
