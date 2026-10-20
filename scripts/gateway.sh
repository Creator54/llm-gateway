# Do not let an inherited Docker endpoint redirect Podman's Compose client.
unset DOCKER_HOST DOCKER_CONTEXT
command=${1:-help}
case "$command" in
  help|--help|-h)
    printf '%s\n' 'Usage: llm-gateway {init|up|down|status|logs}' \
      'Set OCX_IMAGE and HEADROOM_IMAGE in ~/.local/share/llm-gateway/images.env.' \
      'Use published image digests. Initialize the API key before startup.'
    exit 0
    ;;
  init|up|down|status|logs) ;;
  *) echo "Unknown command: $command" >&2; exit 2 ;;
esac
if [ "$#" -gt 1 ]; then
  echo 'Only one command is accepted.' >&2
  exit 2
fi
state_dir="${XDG_DATA_HOME:-$HOME/.local/share}/llm-gateway"
settings="$state_dir/images.env"
if [ ! -f "$settings" ]; then
  echo "Create $settings with OCX_IMAGE and HEADROOM_IMAGE first." >&2
  exit 1
fi
# Compose reads settings, never executable shell code.
compose=(podman compose --project-name llm-gateway --project-directory "$state_dir"
  --env-file "$settings" -f "$GATEWAY_COMPOSE")
case "$command" in
  init) "${compose[@]}" run --rm -T --no-deps hub bun docker/bootstrap-token.ts ;;
  up) "${compose[@]}" up -d --no-build --wait ;;
  down) "${compose[@]}" down ;;
  status) "${compose[@]}" ps ;;
  logs) "${compose[@]}" logs --tail 100 ;;
esac
