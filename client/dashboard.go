package main

import (
	"fmt"
	"image/color"
	"math"
	"strings"
	"time"

	"charm.land/bubbles/v2/progress"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/charmbracelet/x/ansi"
)

type resultMsg struct {
	state  State
	err    error
	action bool
}

type refreshMsg struct{}

type palette struct {
	text, bright, muted, accent, cyan, steel, border, red, green color.Color
	brandBg, brandFg                                             color.Color
	greenBg, amberBg, redBg, blueBg, chipBg                      color.Color
}

func colors(dark bool) palette {
	if dark {
		return palette{
			text:    lipgloss.Color("#D5DFEC"),
			bright:  lipgloss.Color("#FFFFFF"),
			muted:   lipgloss.Color("#7C8DA6"),
			accent:  lipgloss.Color("#F0B429"),
			cyan:    lipgloss.Color("#58A6FF"),
			steel:   lipgloss.Color("#4C7899"),
			border:  lipgloss.Color("#304156"),
			red:     lipgloss.Color("#F87171"),
			green:   lipgloss.Color("#3FB950"),
			brandBg: lipgloss.Color("#E7AE59"),
			brandFg: lipgloss.Color("#0D1117"),
			greenBg: lipgloss.Color("#123524"),
			amberBg: lipgloss.Color("#3B2A0E"),
			redBg:   lipgloss.Color("#42181B"),
			blueBg:  lipgloss.Color("#132C4E"),
			chipBg:  lipgloss.Color("#1E293B"),
		}
	}
	return palette{
		text:    lipgloss.Color("#273447"),
		bright:  lipgloss.Color("#0F172A"),
		muted:   lipgloss.Color("#52647A"),
		accent:  lipgloss.Color("#805005"),
		cyan:    lipgloss.Color("#0969DA"),
		steel:   lipgloss.Color("#3B627E"),
		border:  lipgloss.Color("#A0AAB7"),
		red:     lipgloss.Color("#A52B26"),
		green:   lipgloss.Color("#1A7F37"),
		brandBg: lipgloss.Color("#805005"),
		brandFg: lipgloss.Color("#FFFFFF"),
		greenBg: lipgloss.Color("#DAFBE1"),
		amberBg: lipgloss.Color("#FFF8C5"),
		redBg:   lipgloss.Color("#FFEBE9"),
		blueBg:  lipgloss.Color("#DDF4FF"),
		chipBg:  lipgloss.Color("#E2E8F0"),
	}
}

type dashboard struct {
	client                   *Client
	state                    State
	prevStream               *StreamStatus
	prevAt                   time.Time
	readsPerSec              float64
	servedMiBPerSec          float64
	buildsPerSec             float64
	netMiBPerSec             float64
	width, height, page      int
	selected                 int
	palette                  palette
	loaded, busy, confirm    bool
	confirmStream            bool
	connectionErr, actionErr error
}

func newDashboard(client *Client) dashboard {
	return dashboard{client: client, width: 80, height: 24, selected: -1, palette: colors(true)}
}

func fetchState(client *Client, method string, params any, action bool) tea.Cmd {
	return func() tea.Msg {
		state, _, err := client.call(method, params)
		return resultMsg{state, err, action}
	}
}

func (m dashboard) Init() tea.Cmd {
	return tea.Batch(fetchState(m.client, "status", struct{}{}, false), tea.RequestBackgroundColor)
}

func (m dashboard) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width, m.height = msg.Width, msg.Height
	case tea.BackgroundColorMsg:
		m.palette = colors(msg.IsDark())
	case refreshMsg:
		return m, fetchState(m.client, "status", struct{}{}, false)
	case resultMsg:
		if msg.action {
			m.busy = false
			m.actionErr = msg.err
			if msg.err == nil {
				m.state, m.loaded = msg.state, true
				m.connectionErr = nil
			}
			return m, nil
		}
		m.connectionErr = msg.err
		if msg.err == nil {
			now := time.Now()
			if m.prevStream != nil && msg.state.Stream != nil && !m.prevAt.IsZero() {
				dt := now.Sub(m.prevAt).Seconds()
				if dt >= 0.2 {
					cur, prev := msg.state.Stream, m.prevStream
					if cur.Reads >= prev.Reads {
						m.readsPerSec = float64(cur.Reads-prev.Reads) / dt
					}
					if cur.ServedBytes >= prev.ServedBytes {
						m.servedMiBPerSec = float64(cur.ServedBytes-prev.ServedBytes) / (1024 * 1024 * dt)
					}
					curBuilds := cur.ColdBuilds + cur.LookaheadCompleted
					prevBuilds := prev.ColdBuilds + prev.LookaheadCompleted
					if curBuilds >= prevBuilds {
						m.buildsPerSec = float64(curBuilds-prevBuilds) / dt
					}
					if cur.NetworkBytes >= prev.NetworkBytes {
						m.netMiBPerSec = float64(cur.NetworkBytes-prev.NetworkBytes) / (1024 * 1024 * dt)
					}
				}
			}
			if msg.state.Stream != nil {
				cp := *msg.state.Stream
				m.prevStream = &cp
				m.prevAt = now
			}
			if !m.loaded || len(m.state.Generations) != len(msg.state.Generations) {
				m.selected = len(msg.state.Generations) - 1
			}
			m.state, m.loaded = msg.state, true
		}
		interval := 5 * time.Second
		if (m.state.Job != nil && m.state.Job.State == "running") || (m.state.Stream != nil && m.state.Stream.Mounted) {
			interval = time.Second
		}
		return m, tea.Tick(interval, func(time.Time) tea.Msg { return refreshMsg{} })
	case tea.KeyPressMsg:
		key := msg.String()
		if key == "ctrl+c" || key == "q" {
			return m, tea.Quit
		}
		if m.confirmStream {
			switch key {
			case "esc", "n":
				m.confirmStream = false
			case "enter", "y":
				m.confirmStream, m.busy = false, true
				m.actionErr = nil
				return m, fetchState(m.client, "stream_stop", struct{}{}, true)
			}
			return m, nil
		}
		if m.confirm {
			switch key {
			case "esc", "n":
				m.confirm = false
			case "enter", "y":
				m.confirm, m.busy = false, true
				m.actionErr = nil
				return m, fetchState(m.client, "prepare", struct{}{}, true)
			}
			return m, nil
		}
		switch key {
		case "1":
			if m.state.StreamConfig == nil {
				m.page = 0
			}
		case "2":
			if m.state.StreamConfig == nil {
				m.page = 1
			}
		case "3":
			if m.state.StreamConfig == nil {
				m.page = 2
			}
		case "tab", "right", "l":
			if m.state.StreamConfig == nil {
				m.page = (m.page + 1) % 3
			}
		case "shift+tab", "left", "h":
			if m.state.StreamConfig == nil {
				m.page = (m.page + 2) % 3
			}
		case "down", "j":
			if m.selected+1 < len(m.state.Generations) {
				m.selected++
			}
		case "up", "k":
			if m.selected > 0 {
				m.selected--
			}
		case "p":
			if !m.busy && (m.state.Stream == nil || !m.state.Stream.Mounted) {
				m.confirm = true
			}
		case "v":
			if !m.busy && (m.state.Stream == nil || !m.state.Stream.Mounted) && m.selected >= 0 && m.selected < len(m.state.Generations) {
				m.busy, m.actionErr = true, nil
				params := map[string]string{"generation": m.state.Generations[m.selected].ID}
				return m, fetchState(m.client, "verify", params, true)
			}
		case "s":
			if !m.busy && m.state.StreamConfig != nil {
				if m.state.Stream != nil && m.state.Stream.Mounted {
					m.confirmStream = true
				} else {
					m.busy, m.actionErr = true, nil
					return m, fetchState(m.client, "stream_start", struct{}{}, true)
				}
			}
		case "t":
			if !m.busy && m.state.StreamConfig != nil {
				next := "bing"
				switch m.state.StreamConfig.TileSource {
				case "", "bing":
					next = "arcgis_world_imagery"
				case "arcgis_world_imagery":
					next = "arcgis_clarity"
				case "arcgis_clarity":
					next = "usgs_imagery_only"
				case "usgs_imagery_only":
					next = "bing"
				}
				m.busy, m.actionErr = true, nil
				return m, fetchState(m.client, "stream_source", map[string]any{"tile_source": next}, true)
			}
		case "esc":
			m.actionErr = nil
		}
	}
	return m, nil
}

func (m dashboard) style(c color.Color, text string) string {
	return lipgloss.NewStyle().Foreground(c).Render(text)
}

func (m dashboard) bold(c color.Color, text string) string {
	return lipgloss.NewStyle().Bold(true).Foreground(c).Render(text)
}

func (m dashboard) pill(fg, bg color.Color, text string) string {
	return lipgloss.NewStyle().Bold(true).Foreground(fg).Background(bg).Padding(0, 1).Render(text)
}

func (m dashboard) keycap(key, desc string) string {
	capStyle := lipgloss.NewStyle().Bold(true).Foreground(m.palette.accent).Background(m.palette.chipBg).Padding(0, 1)
	return capStyle.Render(key) + " " + m.style(m.palette.muted, desc)
}

func (m dashboard) label(name string) string {
	return lipgloss.NewStyle().Bold(true).Foreground(m.palette.muted).Width(9).Render(name)
}

func (m dashboard) panel(title, body string, width int) string {
	if width < 40 {
		if title == "" {
			return body
		}
		return m.bold(m.palette.accent, title) + "\n" + body
	}
	content := body
	if title != "" {
		headerBar := m.pill(m.palette.accent, m.palette.chipBg, "▸ "+title)
		content = headerBar + "\n\n" + body
	}
	return lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).
		BorderForeground(m.palette.border).Foreground(m.palette.text).
		Padding(0, 1).Width(width - 2).
		Render(content)
}

func (m dashboard) jobView(width int) string {
	job := m.state.Job
	if job == nil {
		return "No preparation job yet.\n\n" + m.style(m.palette.muted, "p  Compile from configured local sources")
	}
	statusColor := m.palette.accent
	if job.State == "failed" {
		statusColor = m.palette.red
	} else if job.State == "succeeded" {
		statusColor = m.palette.green
	}
	body := m.style(statusColor, strings.ToUpper(safeText(job.Kind)+" / "+safeText(job.State))) +
		"\n" + safeText(job.Stage)
	if job.Total > 0 {
		bar := progress.New(progress.WithColors(m.palette.accent))
		bar.SetWidth(max(10, width-6))
		body += "\n\n" + bar.ViewAs(float64(job.Done)/float64(job.Total))
		unit := "generation checks"
		if job.Kind == "prepare" {
			unit = "terrain definitions compiled"
		}
		body += fmt.Sprintf("\n%d / %d %s", job.Done, job.Total, unit)
	}
	if job.Error != nil {
		body += "\n\n" + m.style(m.palette.red, safeText(*job.Error))
	}
	return body
}

func (m dashboard) generationView(width int) string {
	if len(m.state.Generations) == 0 {
		return m.panel("NO GENERATION PUBLISHED", "Missing or partial assets never become ready.\n\nPrepare a complete local dependency set first.", width)
	}
	var rows []string
	start := max(0, m.selected-3)
	end := min(len(m.state.Generations), start+6)
	for i := start; i < end; i++ {
		g := m.state.Generations[i]
		label := "UNVERIFIED"
		if g.Verified {
			label = "VERIFIED"
		}
		prefix := "  "
		if i == m.selected {
			prefix = "> "
		}
		line := fmt.Sprintf("%s%s  %s  %d px  %.0f MiB", prefix, safeText(g.ID), label, g.Size, float64(g.Bytes)/(1024*1024))
		if i == m.selected {
			line = m.style(m.palette.accent, line)
		}
		rows = append(rows, line)
	}
	if m.selected >= 0 && m.selected < len(m.state.Generations) {
		g := m.state.Generations[m.selected]
		rows = append(rows, "", safeText(g.Package), "", m.style(m.palette.muted, "j/k  Select    v  Verify selected assets"))
	}
	if m.state.Installed != nil {
		rows = append(rows, "", m.style(m.palette.accent, "RECORDED INSTALLATION"), safeText(*m.state.Installed))
	}
	return m.panel("IMMUTABLE GENERATIONS", strings.Join(rows, "\n"), width)
}

func (m dashboard) sourcesView(width int) string {
	if c := m.state.StreamConfig; c != nil {
		sourceLabel := c.TileSource
		if m.state.Stream != nil && m.state.Stream.SourceLabel != "" {
			sourceLabel = m.state.Stream.SourceLabel
		}
		body := safeText(c.SourcePackage) + fmt.Sprintf("\n\nProfile  %d px / complete matched day + night mip chains", c.Size) +
			"\nSource   " + safeText(sourceLabel) + " (" + safeText(c.TileSource) + ")" +
			"\nRevision " + safeText(c.SourceRevision) +
			"\n\nActual texture demand only; no regional compilation.\nCold reads can wait for bounded fetch/build work."
		if s := m.state.Stream; s != nil {
			body += fmt.Sprintf("\n\nOther managed files: %.1f MiB\nBorrowed OSM/VIIRS: %.2f GiB (outside managed ceiling)",
				float64(s.OtherManagedBytes)/(1024*1024), float64(s.BorrowedLightingBytes)/(1024*1024*1024))
			body += fmt.Sprintf("\nKernel mip intersections: %v", s.MipReads)
			body += fmt.Sprintf("\nFirst DDS [offset, bytes] samples: %v", s.DDSReadSamples)
		}
		body += "\n\nOSM emission is inferred, not surveyed per-lamp lighting."
		return m.panel("AUTOMATIC PREVIEW / SOURCE CONTRACT", body, width)
	}
	body := "No source package configured.\n\nUse: groundeffect configure --config FILE"
	if m.state.Config != nil {
		c := m.state.Config
		body = safeText(c.SourceLabel) + "\n\nMesh     " + safeText(c.DSF) +
			fmt.Sprintf("\nOutput   %d x %d pixels, full DDS mip chains", c.Size, c.Size) +
			"\n\nConfiguration validates mesh bindings.\nImagery, OSM and VIIRS are checked during compilation." +
			"\n\nLighting is inferred from normalized OSM roads/buildings.\nVIIRS supplies relative dimming, not surveyed lamp positions."
	}
	body += "\n\n" + m.style(m.palette.muted, "No network or source processing in prepared scenery reads.")
	return m.panel("SOURCE CONTRACT", body, width)
}

func (m dashboard) renderTileCell(state uint8) string {
	switch state {
	case tileLoaded:
		return m.bold(m.palette.green, "■ ")
	case tileTerOnly:
		return m.bold(m.palette.cyan, "■ ")
	case tileBuilding:
		return m.bold(m.palette.accent, "■ ")
	case tileCached:
		return m.style(m.palette.steel, "■ ")
	case tileCold:
		return m.style(m.palette.muted, "· ")
	default:
		return m.style(m.palette.border, "· ")
	}
}

func mergeCellState(a, b uint8) uint8 {
	rank := func(v uint8) int {
		switch v {
		case tileBuilding:
			return 5
		case tileLoaded:
			return 4
		case tileTerOnly:
			return 3
		case tileCached:
			return 2
		case tileCold:
			return 1
		default:
			return 0
		}
	}
	if rank(b) > rank(a) {
		return b
	}
	return a
}

func headingOctant(deg float64) (int, string) {
	arrows := [8]string{"↑", "↗", "→", "↘", "↓", "↙", "←", "↖"}
	norm := math.Mod(deg, 360.0)
	if norm < 0 {
		norm += 360.0
	}
	idx := int(math.Floor((norm+22.5)/45.0)) % 8
	return idx, arrows[idx]
}

func (m dashboard) renderTileGrid(s *StreamStatus, width, maxRows int) string {
	cols, rows := s.TileGridCols, s.TileGridRows
	if cols <= 0 || rows <= 0 || len(s.TileGrid) != cols*rows {
		cols, rows = 16, 16
	}
	innerWidth := max(16, width-6)
	maxColsByWidth := max(6, (innerWidth-2)/2)
	side := min(maxRows, min(maxColsByWidth, max(cols, rows)))
	side = max(6, min(16, side))

	var visGrid [][]uint8
	for r := 0; r < side; r++ {
		r0 := (r * rows) / side
		r1 := max(r0+1, ((r+1)*rows)/side)
		var rowCells []uint8
		for c := 0; c < side; c++ {
			c0 := (c * cols) / side
			c1 := max(c0+1, ((c+1)*cols)/side)
			var cell uint8
			if len(s.TileGrid) == cols*rows {
				for rr := r0; rr < r1 && rr < rows; rr++ {
					for cc := c0; cc < c1 && cc < cols; cc++ {
						cell = mergeCellState(cell, s.TileGrid[rr*cols+cc])
					}
				}
			}
			rowCells = append(rowCells, cell)
		}
		visGrid = append(visGrid, rowCells)
	}
	visRows := len(visGrid)
	visCols := side
	centerRow := visRows / 2
	centerCol := visCols / 2

	arrow := "↑"
	arrowStyle := lipgloss.NewStyle().Bold(true).Foreground(m.palette.text).Background(m.palette.border)
	if s.TelemetryState != "" && s.TelemetryState != "unavailable" && s.AircraftLat != nil && s.AircraftLon != nil {
		trk := 0.0
		if s.TrackDeg != nil {
			trk = *s.TrackDeg
		}
		_, arrow = headingOctant(trk)
		if s.TelemetryState == "fresh" {
			arrowStyle = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("#FFFFFF")).Background(lipgloss.Color("#1F6FEB"))
		} else {
			arrowStyle = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("#0D1117")).Background(m.palette.accent)
		}
	}

	var out []string
	for rIdx, rowCells := range visGrid {
		var line strings.Builder
		line.WriteString("  ")
		for cIdx := 0; cIdx < len(rowCells); cIdx++ {
			if rIdx == centerRow && cIdx == centerCol {
				line.WriteString(arrowStyle.Render(arrow) + " ")
				continue
			}
			line.WriteString(m.renderTileCell(rowCells[cIdx]))
		}
		out = append(out, line.String())
	}
	return strings.Join(out, "\n")
}

func padRightVisual(s string, width int) string {
	w := lipgloss.Width(s)
	if w >= width {
		return ansi.Truncate(s, width, "")
	}
	return s + strings.Repeat(" ", width-w)
}

func (m dashboard) streamView(width int) string {
	s := m.state.Stream
	if s == nil {
		return m.panel("", "No runtime status available.", width)
	}
	innerW := max(20, width-4)
	var body string
	if !s.Mounted {
		body = m.keycap("s", "Start the configured reader")
	} else {
		showHeaders := m.height >= 26
		sectionHdr := func(title string) string {
			return m.bold(m.palette.accent, "━━ "+title)
		}

		var leftLines []string
		if showHeaders {
			leftLines = append(leftLines, sectionHdr("FLIGHT TELEMETRY"))
		}
		if s.TelemetryState != "" && s.TelemetryState != "unavailable" && s.AircraftLat != nil && s.AircraftLon != nil {
			gsKts, aglFt, trk := 0.0, 0.0, 0.0
			if s.GroundSpeedMPS != nil {
				gsKts = *s.GroundSpeedMPS * 1.94384
			}
			if s.AircraftAGLM != nil {
				aglFt = *s.AircraftAGLM * 3.28084
			}
			if s.TrackDeg != nil {
				trk = *s.TrackDeg
			}
			_, trkArrow := headingOctant(trk)
			tFg, tBg := m.palette.green, m.palette.greenBg
			if s.TelemetryState != "fresh" {
				tFg, tBg = m.palette.accent, m.palette.amberBg
			}
			phaseBadge := m.pill(tFg, tBg, fmt.Sprintf("%s/%s", strings.ToUpper(safeText(s.TelemetryState)), strings.ToUpper(safeText(s.FlightPhase))))
			leftLines = append(leftLines,
				m.label("FLIGHT")+phaseBadge+" "+m.bold(m.palette.bright, fmt.Sprintf("%.2f, %.2f", *s.AircraftLat, *s.AircraftLon)),
				m.label("MOTION")+m.bold(m.palette.bright, fmt.Sprintf("%.0fkt", gsKts))+
					m.style(m.palette.text, fmt.Sprintf("  %.0fft AGL  ", aglFt))+
					m.bold(m.palette.cyan, fmt.Sprintf("%03.0f° %s", trk, trkArrow)),
			)
		} else {
			leftLines = append(leftLines,
				m.label("FLIGHT")+m.pill(m.palette.muted, m.palette.chipBg, "UDP 49000 WAITING"),
				m.label("MOTION")+m.style(m.palette.muted, "FUSE demand order"),
			)
		}

		hitRate := 100.0
		if total := s.CacheHits + s.ColdBuilds; total > 0 {
			hitRate = float64(s.CacheHits) * 100.0 / float64(total)
		}
		hitColor := m.palette.green
		if hitRate < 85.0 {
			hitColor = m.palette.accent
		}
		if showHeaders {
			leftLines = append(leftLines, "", sectionHdr("SIMULATOR DEMAND & FUSE"))
		}
		leftLines = append(leftLines,
			m.label("SERVED")+
				m.bold(m.palette.bright, fmt.Sprintf("%.1f MiB/s", m.servedMiBPerSec))+
				m.style(m.palette.muted, fmt.Sprintf(" (%d kernel reads)", s.Reads)),
			m.label("DEMAND")+
				m.bold(hitColor, fmt.Sprintf("%.0f%% hit", hitRate))+
				m.style(m.palette.muted, fmt.Sprintf(" (%d hit / %d cold)", s.CacheHits, s.ColdBuilds)),
		)
		if showHeaders {
			leftLines = append(leftLines, "", sectionHdr("PREDICTIVE LOOKAHEAD"))
		}
		workColor := m.palette.text
		if s.Working > 0 {
			workColor = m.palette.accent
		}
		leftLines = append(leftLines,
			m.label("WORKERS")+
				m.bold(workColor, fmt.Sprintf("%d building", s.Working))+
				m.style(m.palette.muted, fmt.Sprintf(" / %d waiting", s.Pending)),
			m.label("PREFETCH")+
				m.bold(m.palette.cyan, fmt.Sprintf("%dq", s.LookaheadQueued))+
				m.style(m.palette.text, fmt.Sprintf(" / %d built", s.LookaheadCompleted))+
				m.style(m.palette.muted, fmt.Sprintf(" / %d drop", s.LookaheadCancelled)),
			m.label("NETWORK")+
				m.bold(m.palette.bright, fmt.Sprintf("%.2f MiB/s", m.netMiBPerSec))+
				m.style(m.palette.muted, fmt.Sprintf(" (%.1f tiles/s)", m.buildsPerSec)),
		)
		if showHeaders {
			leftLines = append(leftLines, "", sectionHdr("STORAGE & COVERAGE"))
		}
		cachePrefix := ""
		if s.BudgetBytes > 0 && width >= 88 {
			cacheRatio := min(1.0, float64(s.CacheBytes)/float64(s.BudgetBytes))
			cbar := progress.New(progress.WithColors(m.palette.cyan), progress.WithoutPercentage())
			cbar.SetWidth(8)
			cachePrefix = cbar.ViewAs(cacheRatio) + " "
		}
		leftLines = append(leftLines,
			m.label("CACHE")+
				cachePrefix+
				m.bold(m.palette.bright, fmt.Sprintf("%.0f/%.0f MiB",
					float64(s.CacheBytes)/(1024*1024), float64(s.BudgetBytes)/(1024*1024)))+
				m.style(m.palette.muted, fmt.Sprintf(" (%d, %d proc)", s.CachedAssets, s.ActiveConsumers)),
		)

		if s.PreparedTiles > 0 && width >= 44 {
			loadRatio := 0.0
			if s.TerLoadedTiles > 0 {
				denom := s.TerLoadedTiles
				if s.Pending == 0 && s.Working == 0 && s.LoadedTiles > 0 {
					denom = s.LoadedTiles
				}
				loadRatio = min(1.0, float64(s.LoadedTiles)/float64(max(1, denom)))
			} else if s.CachedTiles > 0 {
				loadRatio = min(1.0, float64(s.CachedTiles)/float64(s.PreparedTiles))
			}
			bar := progress.New(progress.WithColors(m.palette.green), progress.WithoutPercentage())
			bar.SetWidth(8)
			leftLines = append(leftLines,
				m.label("LOAD")+
					fmt.Sprintf("%s %s %s",
						bar.ViewAs(loadRatio),
						m.bold(m.palette.green, fmt.Sprintf("%3.0f%%", loadRatio*100)),
						m.style(m.palette.muted, fmt.Sprintf("(%d/%d dds)", s.LoadedTiles, s.TerLoadedTiles))),
			)
		} else {
			leftLines = append(leftLines, m.label("LOAD")+m.style(m.palette.muted, "Waiting for DSF cells..."))
		}

		if m.height >= 22 && width >= 68 {
			maxBodyLines := max(10, m.height-11)
			framedRadar := m.height >= 26 && width >= 84
			borderRows := 0
			if framedRadar {
				borderRows = 2
			}
			maxGridRows := max(6, min(16, maxBodyLines-1-borderRows))
			gridWidth := maxGridRows*2 + 4
			if framedRadar {
				gridWidth += 2
			}
			leftW := max(36, innerW-gridWidth-2)
			gridStr := m.renderTileGrid(s, gridWidth+6, maxGridRows)
			legend := m.pill(m.palette.accent, m.palette.chipBg, "GRID") + " " +
				m.bold(m.palette.green, "■") + m.style(m.palette.muted, "load ") +
				m.bold(m.palette.cyan, "■") + m.style(m.palette.muted, "ter ") +
				m.bold(m.palette.accent, "■") + m.style(m.palette.muted, "bld ") +
				m.style(m.palette.steel, "■") + m.style(m.palette.muted, "cache ") +
				m.style(m.palette.muted, "·cold")
			if framedRadar {
				trimmedGrid := strings.ReplaceAll("\n"+gridStr, "\n  ", "\n")
				trimmedGrid = strings.TrimPrefix(trimmedGrid, "\n")
				radarBox := lipgloss.NewStyle().
					Border(lipgloss.RoundedBorder()).
					BorderForeground(m.palette.border).
					Padding(0, 1).
					Render(trimmedGrid)
				rightBlock := legend + "\n" + radarBox
				if len(leftLines) > maxBodyLines {
					leftLines = leftLines[:maxBodyLines]
				}
				for i := range leftLines {
					leftLines[i] = padRightVisual(leftLines[i], leftW)
				}
				joined := lipgloss.JoinHorizontal(lipgloss.Top, strings.Join(leftLines, "\n"), "  ", rightBlock)
				joinedLines := strings.Split(joined, "\n")
				if len(joinedLines) > maxBodyLines {
					joinedLines = joinedLines[:maxBodyLines]
				}
				body = strings.Join(joinedLines, "\n")
			} else {
				rightLines := append([]string{legend}, strings.Split(gridStr, "\n")...)
				totalRows := max(len(leftLines), len(rightLines))
				if totalRows > maxBodyLines {
					totalRows = maxBodyLines
				}
				var rows []string
				for i := 0; i < totalRows; i++ {
					l, r := "", ""
					if i < len(leftLines) {
						l = leftLines[i]
					}
					if i < len(rightLines) {
						r = rightLines[i]
					}
					line := padRightVisual(l, leftW)
					if r != "" {
						line += "  " + ansi.Truncate(r, max(10, innerW-leftW-2), "")
					}
					rows = append(rows, line)
				}
				body = strings.Join(rows, "\n")
			}
		} else {
			var rows []string
			for _, l := range leftLines {
				rows = append(rows, ansi.Truncate(l, innerW, ""))
			}
			body = strings.Join(rows, "\n")
		}
	}
	if s.LastError != nil {
		body += "\n\n" + m.pill(m.palette.red, m.palette.redBg, "LAST READER ERROR: "+safeText(*s.LastError))
	}
	return m.panel("", body, width)
}

func (m dashboard) View() tea.View {
	width := max(16, min(120, m.width-4))
	connection, connFg, connBg := "ENGINE CONNECTING", m.palette.muted, m.palette.chipBg
	if m.connectionErr != nil {
		connection, connFg, connBg = "ENGINE DISCONNECTED", m.palette.red, m.palette.redBg
	} else if m.loaded {
		connection, connFg, connBg = "ENGINE CONNECTED", m.palette.green, m.palette.greenBg
	}
	head := m.pill(m.palette.brandFg, m.palette.brandBg, "GROUNDEFFECT") + " " +
		m.pill(connFg, connBg, connection)
	if m.loaded && m.state.Stream != nil {
		s := m.state.Stream
		if s.Mounted {
			head += " " + m.pill(m.palette.green, m.palette.greenBg, "● READER MOUNTED")
			if s.Pending == 0 {
				head += " " + m.pill(m.palette.green, m.palette.greenBg, "✓ 0 BLOCKING")
			} else {
				head += " " + m.pill(m.palette.accent, m.palette.amberBg, fmt.Sprintf("▲ %d WAITING", s.Pending))
			}
			if s.SourceLabel != "" && width >= 88 {
				head += " " + m.pill(m.palette.cyan, m.palette.blueBg, safeText(s.SourceLabel))
			}
		} else {
			head += " " + m.pill(m.palette.muted, m.palette.chipBg, "○ READER STOPPED")
		}
	} else if m.loaded {
		installation := "INSTALL: NONE"
		instFg, instBg := m.palette.muted, m.palette.chipBg
		if m.state.Installed != nil {
			installation = "INSTALL RECORDED"
			instFg, instBg = m.palette.cyan, m.palette.blueBg
		}
		if m.state.StreamInstalled != nil {
			installation = "STREAM INSTALL RECORDED"
			instFg, instBg = m.palette.cyan, m.palette.blueBg
		}
		head += " " + m.pill(instFg, instBg, installation)
	}

	if m.loaded && m.state.Stream != nil {
		s := m.state.Stream
		var consumerBadge string
		if s.LastReader != nil {
			consumerBadge = m.pill(m.palette.green, m.palette.greenBg, "● READER ACTIVE") + " " +
				m.style(m.palette.text, "Observed: "+safeText(*s.LastReader))
		} else {
			consumerBadge = m.pill(m.palette.muted, m.palette.chipBg, "○ READER IDLE") + " " +
				m.style(m.palette.muted, "No file reader observed yet.")
		}
		head += "\n" + consumerBadge
	} else {
		head += "\n" + m.bold(m.palette.muted, "LINUX / ALPHA 01")
	}

	if m.state.StreamConfig == nil {
		tabs := []string{"1 Flight deck", "2 Generations", "3 Sources"}
		for i := range tabs {
			if i == m.page {
				tabs[i] = m.pill(m.palette.brandFg, m.palette.brandBg, tabs[i])
			} else {
				tabs[i] = m.pill(m.palette.muted, m.palette.chipBg, tabs[i])
			}
		}
		head += "\n" + strings.Join(tabs, "  ") + "\n"
	} else {
		head += "\n"
	}
	var body string
	if m.confirmStream {
		body = m.panel("STOP THE AUTOMATIC READER?", "Exit X-Plane and other readers first.\nThe engine refuses stop while files/processes are in use.\n\n"+m.keycap("enter", "Request stop")+"   "+m.keycap("esc", "Cancel"), width)
	} else if m.confirm {
		body = m.panel("PREPARE A NEW GENERATION?", "Compile complete base + night textures from local inputs.\nExisting generations remain unchanged.\n\n"+m.keycap("enter", "Prepare")+"   "+m.keycap("esc", "Cancel"), width)
	} else if !m.loaded {
		body = m.panel("CONNECTING", "Waiting for authoritative engine state.", width)
	} else if m.state.StreamConfig != nil {
		body = m.streamView(width)
	} else {
		switch m.page {
		case 1:
			body = m.generationView(width)
		case 2:
			body = m.sourcesView(width)
		default:
			config := "No scenery source configured.\nUse configure --config FILE before preparing."
			if m.state.Config != nil {
				config = m.bold(m.palette.bright, safeText(m.state.Config.SourceLabel)) + "\n" + safeText(m.state.Config.DSF)
			}
			verified := 0
			for _, g := range m.state.Generations {
				if g.Verified {
					verified++
				}
			}
			config += fmt.Sprintf("\n\n%d verified / %d published generations\nDelivery: local native files", verified, len(m.state.Generations))
			if m.height < 30 && m.state.Config != nil {
				config = m.bold(m.palette.bright, safeText(m.state.Config.SourceLabel)) +
					fmt.Sprintf("\n%d px / %d verified / native files", m.state.Config.Size, verified)
			}
			body = m.panel("SCENERY / OFFLINE FIRST", config, width) +
				"\n" + m.panel("PREPARATION", m.jobView(width-2), width)
		}
	}
	if m.connectionErr != nil {
		head += m.pill(m.palette.red, m.palette.redBg, "DISPLAYED STATE MAY BE STALE") + "\n"
		body += "\n" + m.style(m.palette.red, safeText(m.connectionErr.Error()))
	}
	if m.actionErr != nil {
		body += "\n" + m.style(m.palette.red, safeText(m.actionErr.Error()))
	}
	footer := m.keycap("p", "Prepare") + "   " + m.keycap("v", "Verify") + "   " + m.keycap("tab", "Navigate") + "   " + m.keycap("q", "Quit UI")
	if m.state.StreamConfig != nil {
		footer = m.keycap("s", "Reader start/stop") + "   " + m.keycap("t", "Tile source") + "   " + m.keycap("q", "Quit UI only")
	}
	if m.width >= 70 {
		if m.state.StreamConfig != nil {
			footer += "\n" + m.style(m.palette.muted, "Mounted is not simulator consumption. q leaves the reader running.")
		} else {
			footer += "\n" + m.style(m.palette.muted, "Native files; no live X-Plane connection. q closes only the UI.")
		}
	}
	lines := strings.Split(head+body, "\n")
	height := max(1, m.height-5)
	if len(lines) > height {
		lines = lines[:height]
	}
	for len(lines) < height {
		lines = append(lines, "")
	}
	content := strings.Join(lines, "\n") + "\n" + footer
	rendered := strings.Split(content, "\n")
	for i := range rendered {
		rendered[i] = ansi.Truncate(rendered[i], width, "")
	}
	content = strings.Join(rendered, "\n")
	view := tea.NewView(lipgloss.NewStyle().Padding(1, 2).Render(content))
	view.AltScreen = true
	view.WindowTitle = "GroundEffect"
	return view
}
