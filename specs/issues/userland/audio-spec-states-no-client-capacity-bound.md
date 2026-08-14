# The audio spec states no client capacity bound

`specs/audio-subsystem-spec.md` accepts "multiple simultaneous clients" and
never bounds them. Every client costs a shared-memory ring, a pipe and mix
work per period, so an unbounded acceptor is a resource exhaustion path, and
the spec cannot state the refusal a client past the bound observes because no
bound exists to state.

Decide the bound (or derive it from an existing resource bound), implement
the refusal, and state both in the spec.
