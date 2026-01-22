package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"time"
)

const (
	defaultOutputFile  = "vm.state"
	defaultWaitString  = "=========="
	defaultTimeout     = 5 * time.Minute
	progressInterval   = 10 * time.Second
)

func main() {
	var (
		outputFile  = flag.String("output", defaultOutputFile, "path to output state file")
		argsJSON    = flag.String("args-json", "", "path to json file containing args")
		timeout     = flag.Duration("timeout", defaultTimeout, "timeout for waiting for marker")
	)

	flag.Parse()
	args := flag.Args()

	log.Printf("get-qemu-state: timeout=%v, output=%s", *timeout, *outputFile)

	if *outputFile == "" {
		log.Fatalf("output file must not be empty")
	}
	if *argsJSON == "" {
		log.Fatalf("specify args JSON")
	}

	var extraArgs []string
	argsData, err := os.ReadFile(*argsJSON)
	if err != nil {
		log.Fatalf("failed to get args json: %v", err)
	}
	if err := json.Unmarshal(argsData, &extraArgs); err != nil {
		log.Fatalf("failed to parse args json: %v", err)
	}
	log.Println(extraArgs)

	// Create context with timeout
	ctx, cancel := context.WithTimeout(context.Background(), *timeout)
	defer cancel()

	log.Printf("Starting QEMU: %s", args[0])
	cmd := exec.CommandContext(ctx, args[0], extraArgs...)

	stdin, err := cmd.StdinPipe()
	if err != nil {
		log.Fatal(err)
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		log.Fatal(err)
	}

	cmd.Stderr = os.Stderr

	startTime := time.Now()
	if err := cmd.Start(); err != nil {
		log.Fatalf("failed to start: %v", err)
	}
	log.Printf("QEMU started (PID %d)", cmd.Process.Pid)

	// Progress reporter
	progressTicker := time.NewTicker(progressInterval)
	go func() {
		bytesRead := 0
		for {
			select {
			case <-progressTicker.C:
				elapsed := time.Since(startTime).Round(time.Second)
				log.Printf("Still waiting for marker... (elapsed: %v, bytes read: %d)", elapsed, bytesRead)
			case <-ctx.Done():
				return
			}
		}
	}()

	snapshotCh := make(chan struct{})
	doneCh := make(chan struct{})
	errorCh := make(chan error, 1)

	// Snapshot goroutine - triggers migration after marker detected
	go func() {
		select {
		case <-snapshotCh:
			// Marker detected, start migration
		case <-ctx.Done():
			return
		}

		log.Println("Entering QEMU monitor mode (Ctrl-A C)")
		_, err := stdin.Write([]byte{byte(0x01), byte('c')}) // send Ctrl-A C to start the monitor mode
		if err != nil {
			errorCh <- fmt.Errorf("failed to start monitor: %w", err)
			return
		}

		log.Printf("Sending migrate command: migrate file:%s", *outputFile)
		for {
			if _, err := io.WriteString(stdin, fmt.Sprintf("migrate file:%s\n", *outputFile)); err != nil {
				errorCh <- fmt.Errorf("failed to invoke migrate: %w", err)
				return
			}
			time.Sleep(500 * time.Millisecond)
			if fi, err := os.Stat(*outputFile); err == nil {
				log.Printf("State file created: %s (%d bytes)", *outputFile, fi.Size())
				break // state file exists
			} else if !errors.Is(err, os.ErrNotExist) {
				errorCh <- fmt.Errorf("failed to stat state file: %w", err)
				return
			}
		}

		log.Println("Finishing QEMU (sending quit)")
		if _, err := io.WriteString(stdin, "quit\n"); err != nil {
			errorCh <- fmt.Errorf("failed to invoke quit: %w", err)
			return
		}
		close(doneCh)
	}()

	// Marker detection goroutine - reads stdout looking for "=========="
	go func() {
		p := make([]byte, 1)
		cnt := 0
		bytesRead := 0
		for {
			select {
			case <-ctx.Done():
				errorCh <- fmt.Errorf("timeout waiting for marker after %v (read %d bytes)", time.Since(startTime), bytesRead)
				return
			default:
			}

			if _, err := stdout.Read(p); err != nil {
				if ctx.Err() != nil {
					return // Context cancelled
				}
				errorCh <- fmt.Errorf("failed to read stdout: %w", err)
				return
			}
			bytesRead++

			if string(p) == "=" {
				cnt++
			} else {
				cnt = 0
			}
			if cnt == 10 {
				elapsed := time.Since(startTime).Round(time.Millisecond)
				log.Printf("Detected marker '==========' after %v (read %d bytes)", elapsed, bytesRead)
				break // start snapshotting
			}
			if _, err := os.Stdout.Write(p); err != nil {
				errorCh <- fmt.Errorf("failed to copy stdout: %w", err)
				return
			}
		}
		close(snapshotCh)
		if _, err := io.Copy(os.Stdout, stdout); err != nil && ctx.Err() == nil {
			errorCh <- fmt.Errorf("failed to copy stdout: %w", err)
		}
	}()

	// Wait for completion or error
	select {
	case <-doneCh:
		progressTicker.Stop()
		elapsed := time.Since(startTime).Round(time.Millisecond)
		log.Printf("Snapshot capture completed successfully in %v", elapsed)
	case err := <-errorCh:
		progressTicker.Stop()
		cmd.Process.Kill()
		log.Fatalf("Error during snapshot capture: %v", err)
	case <-ctx.Done():
		progressTicker.Stop()
		cmd.Process.Kill()
		log.Fatalf("Timeout after %v waiting for marker", *timeout)
	}

	if err := cmd.Wait(); err != nil {
		// Ignore exit error if we sent quit command
		if exitErr, ok := err.(*exec.ExitError); ok {
			log.Printf("QEMU exited with code %d", exitErr.ExitCode())
		} else {
			log.Fatalf("waiting for qemu: %v", err)
		}
	}
}
