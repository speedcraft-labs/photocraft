# Recursive Gaussian (issue #176)

The CPU Gaussian path in `photocraft-algo` now delegates its old three-box
branch to `craft-blur`'s raw Young–van Vliet quality. Direct sampled kernels
remain unchanged. The filter still works on the current source window, clamps
at its edges, premultiplies alpha before filtering, and crops to the requested
output region afterward. If the shared filter rejects an input, the old box
path remains a fallback.

The old box widths quantized the requested sigma and admitted a relative
variance error up to `1 / (3 sigma)`. The shared recurrence refines its pole
parameter to match the requested second moment. Tests in `craft-blur` compare
its impulse response with a normalized sampled Gaussian: tested discrete
variance error is below 0.5%; maximum absolute shape error is 0.023721 at
sigma 2.5 and 0.011705 at sigma 4, and below 0.01 from sigma 8 onward. These
match the shared crate's 48-case generated-sigma checks: shape error <=0.03 on
[2.5, 8), <=0.01 on [8, 64], and variance error <0.5% on [2.5, 64]. These are
impulse-shape bounds, not arbitrary-image output bounds. See the shared crate's
README for the precise quality and edge contracts.
