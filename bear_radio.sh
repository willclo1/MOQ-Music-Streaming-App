#!/bin/bash

# bear_radio.sh - Script to run Bear Radio components

# Default values
BIND_ADDR="[::]:0"
HOST="localhost"
RELAY_PORT=4443
URL="https://$HOST:$RELAY_PORT"
BASE_PATH="."
LOG_LEVEL="info"

# Function to show usage information
show_usage() {
    echo "Bear Radio - Audio Streaming System"
    echo ""
    echo "Usage: $0 [command] [options]"
    echo ""
    echo "Commands:"
    echo "  relay       Start the relay server"
    echo "  publisher   Start a publisher for a station"
    echo "  subscriber  Start a subscriber for a station"
    echo "  all         Start relay, publisher and subscriber (default station1)"
    echo ""
    echo "Options:"
    echo "  -b, --bind ADDRESS     Bind address (default: [::]:0)"
    echo "  -h, --host HOST        Host address (default: localhost)"
    echo "  -u, --url URL          Server URL (default: https://HOST:PORT)"
    echo "  -s, --song SONG        Song name without extension (required for publisher)"
    echo "  -i, --station INDEX    Station index (1-3, default: 1)"
    echo "  -p, --path PATH        Base path for files (default: current directory)"
    echo "  -l, --log LEVEL        Log level (default: info)"
    echo "  -r, --relay-port PORT  Port for relay server (default: 4443)"
    echo "  --help                 Show this help message"
    echo ""
}

# Function to run the relay server
run_relay() {
    echo "📡 Starting relay server on $HOST:$RELAY_PORT..."
    cd "$BASE_PATH"
    cargo run --bin moq-relay -- --bind "[::]:$RELAY_PORT" --tls-self-sign "$HOST:$RELAY_PORT" --cluster-node "$HOST:$RELAY_PORT" --tls-disable-verify
}

# Function to run a publisher
run_publisher() {
    if [ -z "$SONG" ]; then
        echo "❌ Error: Song name is required for publisher"
        show_usage
        exit 1
    fi

    echo "🎵 Starting publisher for song '$SONG' on station$STATION_INDEX connecting to $URL..."
    cd "$BASE_PATH"
    cargo run --bin bear_radio -- \
        --bind "$BIND_ADDR" \
        --log "$LOG_LEVEL" \
        --song "$SONG" \
        --station-index "$STATION_INDEX" \
        "$URL" \
        publish
}

# Function to run a subscriber
run_subscriber() {
    echo "🔊 Starting subscriber for station$STATION_INDEX connecting to $URL..."
    cd "$BASE_PATH"
    cargo run --bin bear_radio -- \
        --bind "$BIND_ADDR" \
        --log "$LOG_LEVEL" \
        --station-index "$STATION_INDEX" \
        "$URL" \
        subscribe
}

# Parse command
if [ $# -eq 0 ]; then
    show_usage
    exit 0
fi

COMMAND="$1"
shift

# Default station index
STATION_INDEX=1

# Parse options
while [[ $# -gt 0 ]]; do
    case "$1" in
        -b|--bind)
            BIND_ADDR="$2"
            shift 2
            ;;
        -h|--host)
            HOST="$2"
            URL="https://$HOST:$RELAY_PORT"  # Update URL when host changes
            shift 2
            ;;
        -u|--url)
            URL="$2"
            shift 2
            ;;
        -s|--song)
            SONG="$2"
            shift 2
            ;;
        -i|--station)
            STATION_INDEX="$2"
            shift 2
            ;;
        -p|--path)
            BASE_PATH="$2"
            shift 2
            ;;
        -l|--log)
            LOG_LEVEL="$2"
            shift 2
            ;;
        -r|--relay-port)
            RELAY_PORT="$2"
            URL="https://$HOST:$RELAY_PORT"  # Update URL when port changes
            shift 2
            ;;
        --help)
            show_usage
            exit 0
            ;;
        *)
            echo "❌ Unknown option: $1"
            show_usage
            exit 1
            ;;
    esac
done

# Execute command
case "$COMMAND" in
    relay)
        run_relay
        ;;
    publisher)
        run_publisher
        ;;
    subscriber)
        run_subscriber
        ;;
    all)
        # Run all components with default settings
        echo "🐻 Starting Bear Radio system on $HOST:$RELAY_PORT..."

        # Start relay in background
        run_relay &
        RELAY_PID=$!
        echo "Relay started with PID: $RELAY_PID"

        # Wait for relay to initialize
        sleep 2

        # Start publisher in background if song is provided
        if [ -n "$SONG" ]; then
            run_publisher &
            PUB_PID=$!
            echo "Publisher started with PID: $PUB_PID"

            # Wait for publisher to initialize
            sleep 2
        else
            echo "⚠️ No song specified, skipping publisher"
        fi

        # Start subscriber in foreground
        run_subscriber

        # Clean up background processes on exit
        trap "kill $RELAY_PID $PUB_PID 2>/dev/null" EXIT
        ;;
    *)
        echo "❌ Unknown command: $COMMAND"
        show_usage
        exit 1
        ;;
esac

exit 0