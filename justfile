set unstable := true

GIT_SHA := `git describe --abbrev=40 --always --dirty --match=nevermatch 2>/dev/null`

default:
    @just --list

[no-exit-message]
[positional-arguments]
run *args:
    #!/usr/bin/env bash
    if [ "$#" -eq 0 ]; then
      echo "Usage: run <task> [arguments...]" >&2
      exit 1
    fi

    # Store all command-line arguments in an array.
    args=("$@")
    num_args=$#

    found=""
    found_index=0

    # Try the longest possible prefix down to a single argument.
    for (( i = num_args; i > 0; i-- )); do
      candidate="./tasks"
      for (( j = 0; j < i; j++ )); do
        candidate="${candidate}/${args[j]}"
      done
      if [ -f "$candidate" ] && [ -x "$candidate" ]; then
        found="$candidate"
        found_index=$i
        break
      fi
    done

    if [ -z "$found" ]; then
      echo "Error: No valid executable found matching the given arguments in ./tasks." >&2
      exit 1
    fi

    # Execute the found file with any remaining arguments.
    exec "$found" "${args[@]:$found_index}"

test:
    cargo nextest run
    cargo test --doc

# Run the reproducible end-to-end speed and storage benchmark.
benchmark profile='ci' samples='5' warmups='1' output='target/benchmark/current.json':
    cargo build --release --locked -p graft-cli -p graft-bench
    ./target/release/graft-bench run \
      --graft-bin ./target/release/graft \
      --output {{ quote(output) }} \
      --label "{{ GIT_SHA }}" \
      --profile {{ quote(profile) }} \
      --samples {{ quote(samples) }} \
      --warmups {{ quote(warmups) }}

# Run a single small sample to validate the benchmark harness.
benchmark-smoke:
    just benchmark smoke 1 0 target/benchmark/smoke.json

# Compare two benchmark JSON reports and produce Markdown.
benchmark-compare baseline candidate output='target/benchmark/comparison.md':
    cargo run --locked -p graft-bench --release -- compare \
      --baseline {{ quote(baseline) }} \
      --candidate {{ quote(candidate) }} \
      --output {{ quote(output) }}

build-all:
    cargo build
