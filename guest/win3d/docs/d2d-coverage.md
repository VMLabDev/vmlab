# Direct2D coverage: forced-sample-count draws without the interlock

Direct2D on feature level 11.1 antialiases filled shapes with
target-independent rasterization. It draws triangle fans with
`ForcedSampleCount` 16 into a single-sampled `R16_UINT` texture, the pixel
shader writes `SV_Coverage`, and the output merger XORs it into the texture.
A second pass turns the 16 coverage bits of each pixel into an alpha.

## The fault

Upstream's driver draws the coverage pass once, at 16 samples with no
attachment, and does the XOR in the fragment shader under a pixel interlock
(`VK_EXT_fragment_shader_interlock`; `yttrium_pipeline.c`,
`yttrium_shader.c`). Where the lock does not hold between two fragments of
one pixel whose coverage is disjoint, one of the two XORs is lost. That is
every pixel on an edge that two triangles of a fan share. Microsoft Word's
ribbon showed short pale gaps in its rounded boxes, in other places at every
start (GitHub #236, found on a laptop RTX 4050 with package 0.1.2-alpha).

## The fix

`patches/mesa/0094` sends such a draw as one single-sampled draw per sample,
with the texture as an ordinary colour attachment and the blender's logic op
XOR. No lock is needed, because the blender works in primitive order.

- For sample *i* the viewport is moved so that the pixel centre falls on that
  sample's position (D3D11's standard pattern).
- One pipeline serves all the draws: each draw's viewport has its depth range
  collapsed to (*i* + 0.5) / 16, and the fragment shader takes its
  `SV_Coverage` bit from the fragment depth.
- It is taken only for what Direct2D does here: one `R16_UINT`
  single-sampled target, logic op XOR, no blending, no depth buffer, and a
  shader that reads the sample mask and does no per-sample shading.
  Everything else goes the old way.

`FORCED_SAMPLE_PASSES=0` in `C:\ProgramData\Yttrium\yttrium.ini` (or
`D3D10UMD_YTTRIUM_FORCED_SAMPLE_PASSES=0` in the environment) brings the
interlock path back. The setting is read once per process.

Limits:

- 16 draws stand in for one. The cost is not measured.
- Indirect draws still take the interlock path.
