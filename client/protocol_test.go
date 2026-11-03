package main

import (
	"encoding/json"
	"net"
	"path/filepath"
	"strings"
	"testing"
)

func testServer(t *testing.T, response func(string) string) *Client {
	t.Helper()
	dir := t.TempDir()
	listener, err := net.Listen("unix", filepath.Join(dir, "control.sock"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { listener.Close() })
	done := make(chan struct{})
	go func() {
		defer close(done)
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		var request struct{ ID string }
		if json.NewDecoder(conn).Decode(&request) != nil {
			return
		}
		_, _ = conn.Write([]byte(response(request.ID) + "\n"))
	}()
	t.Cleanup(func() { listener.Close(); <-done })
	return &Client{dataDir: dir}
}

func TestProtocol(t *testing.T) {
	tests := []struct {
		name, response string
		wantError      bool
	}{
		{"valid", `{"version":1,"id":"ID","result":{"version":1,"generations":[]},"error":null}`, false},
		{"version", `{"version":2,"id":"ID","result":{"version":1},"error":null}`, true},
		{"id", `{"version":1,"id":"wrong","result":{"version":1},"error":null}`, true},
		{"failure", `{"version":1,"id":"ID","result":null,"error":"source incomplete"}`, true},
		{"malformed", `{"version":`, true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			client := testServer(t, func(id string) string { return strings.ReplaceAll(test.response, "ID", id) })
			_, _, err := client.call("status", struct{}{})
			if (err != nil) != test.wantError {
				t.Fatalf("error=%v; wantError=%t", err, test.wantError)
			}
		})
	}
}

func TestMessageBounds(t *testing.T) {
	client := testServer(t, func(string) string { return strings.Repeat("x", maxMessage+1) })
	if _, _, err := client.call("status", struct{}{}); err == nil {
		t.Fatal("oversized response accepted")
	}
	client = testServer(t, func(string) string { return "" })
	if _, _, err := client.call("configure", strings.Repeat("x", maxMessage+1)); err == nil {
		t.Fatal("oversized request accepted")
	}
}

func TestTerminalSanitization(t *testing.T) {
	value := "\x1b]52;c;payload\a\nname\u202e"
	result := safeText(value)
	if strings.ContainsAny(result, "\x1b\a\n\u202e") {
		t.Fatalf("terminal controls remain: %q", result)
	}
}
