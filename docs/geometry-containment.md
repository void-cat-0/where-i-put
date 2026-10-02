# Geometry and containment evidence model

> Status: G1–G4 are implemented (G4 on the read side, from a priors file); G5
> (query/UI evidence cards) remains future work.
>
> This document is both the design contract and the implementation boundary for
> the inference layer: using image geometry, relative depth, object size, time,
> and container semantics to produce cautious hypotheses. The implemented G1–G3
> path currently persists bounded geometry/lifecycle evidence, derives
> conservative 2-D visibility candidates, and optionally records ordinal
> relative-depth evidence. It does **not** establish physical containment or
> calibrated VLM accuracy. A deployment may have no depth provider or an
> optional monocular relative-depth provider; metric depth, size, and affordance
> evidence remain future extensions.

## 1. Problem statement

The current pipeline records a deduplicated `Observation`:

> label X was visible at camera C / zone Z during `[first_seen, last_seen]`.

G1 now also persists bounded `ObservationGeometry` samples, authoritative
`ObservationEvent` lifecycle rows, and optional `DepthEvidence` in SQLite. The
store keeps first/last geometry and immutable event-boundary copies; it does not
store raw per-frame detections. That evidence is enough for the landed G2
visibility candidates, but not enough to claim physical containment. A useful
containment or occlusion hypothesis needs more evidence:

```text
object A disappears
+ object B appears or persists nearby
+ their projected image regions overlap or are adjacent
+ their relative depth is compatible
+ B has container/cover semantics
+ A could physically fit in B (if the evidence is available)
+ the timing is plausible
```

The result is an inference, not a new observation. Raw facts must remain
unchanged and auditable.

## 2. Design principles

### 2.1 Facts and hypotheses are separate

`Observation`, detections, depth measurements, and event timestamps are facts
reported by an ingest path. `Inference` is a derived claim produced by a query
or rule layer. An inference must never rewrite an observation's label, zone,
position, or snapshot.

### 2.2 Unknown is not false

A camera without depth, calibration, or a reliable size estimate must produce
`unknown`, not a fabricated zero or a negative conclusion. In particular:

- no depth provider means depth evidence is unavailable;
- monocular depth normally establishes ordering, not centimetres;
- a 2-D box is not a 3-D size measurement;
- an unrecognised container opening is not proof that containment is
  impossible.

### 2.3 Store intervals and provenance, not false precision

Physical measurements use a value plus uncertainty or an interval. Every
measurement records its source and coordinate convention. A value such as
`0.43 m` without calibration and uncertainty is not acceptable as a metric
measurement.

### 2.4 Start with one fixed camera

The first implementation targets one fixed camera and one camera coordinate
frame. Cross-camera identity, automatic re-identification, and multi-view
reconstruction are explicitly out of scope until this model has real evidence.

### 2.5 Inferences are reproducible

A derived relation records the rule/model version and the evidence ids used to
produce it. Re-running a newer rule version may create a new inference; it must
not silently mutate the historical result.

## 3. Vocabulary

### 3.1 Geometry

A 2-D image-space bounding box is `[x_min, y_min, x_max, y_max]` in pixels. A
3-D point or extent is expressed in a named camera coordinate frame. The model
does not assume that a monocular depth value is metric.

### 3.2 Relative depth

Relative depth expresses ordering or coarse separation:

```text
nearer, farther, same_plane, overlapping_depth, unknown
```

It is useful even when scale is unknown. `nearer` means nearer to the camera,
not nearer to the target object.

### 3.3 Metric depth

Metric depth is a distance or 3-D extent in a calibrated unit, accompanied by
uncertainty and calibration provenance. It may come from stereo, ToF/RGB-D, or
a calibrated scene model. A monocular estimator may provide it only if an
explicit scale calibration step exists; otherwise it stays relative.

### 3.4 Container and cover

A **container** has a plausible interior or opening: box, drawer, bag, cabinet,
basket, and similar objects. A **cover** can hide another object without
necessarily containing it: book, lid, cloth, tray, or a person. One label may
be both depending on state and pose.

The system must not infer `inside` merely because two 2-D boxes overlap.

## 4. Evidence records

The following are logical records. G1 implements separate SQLite tables for
`observation_geometry`, `observation_events`, and `depth_evidence`, with the
relationships shown below. G4/G5 records remain a forward-compatible design
contract; they are not currently persisted.

### 4.1 Observation geometry

Each observation may have bounded geometry samples. SQLite does not store every
decoded frame: ingest maintains `first` and `last` samples, and lifecycle events
freeze immutable event-boundary copies when needed. Event-boundary samples may
therefore have kinds such as `event_appeared_*` or `event_disappeared_*`; they
are evidence copies, not a per-frame history.

```text
ObservationGeometry (implemented G1 fields)
--------------------------------------------
id                         stable evidence id
observation_id            FK to observations.id
sample_kind               first / last / immutable event-boundary kind
captured_at               UTC timestamp
bbox                      [x0, y0, x1, y1] in source pixels
frame_width               optional source width
frame_height              optional source height
zone                       zone resolved for this sample
source                     local_detector / frigate / manual
confidence                detector confidence, if supplied
snapshot_ref              optional evidence snapshot
```

The existing representative observation snapshot remains useful for the UI,
but a relation should be able to point at the exact event-boundary snapshot or
frame that supplied its geometry.

G1 retains:

- the moving `first` geometry sample;
- the moving `last` geometry sample;
- immutable geometry copies associated with `appeared` and `disappeared`
  lifecycle events when supplied;
- the sample used by a G2 relation, when the rule selects one.

This bounded policy is intentional: the DB stores first/last/event geometry,
not raw per-frame detections.

### 4.2 Depth evidence

G3 optionally attaches depth to a geometry sample, not directly to a label. This
avoids pretending that one depth value describes an object for its whole
observation span. The current provider is ordinal only: it reports
`nearer`/`middle`/`farther`/`unknown`, an optional within-response score, quality,
and provider/model/request provenance. It does not produce metres, calibration,
or a physical containment decision. Missing, invalid, or incomparable depth is
`unknown`.

```text
DepthEvidence
-------------
id                         stable evidence id
geometry_id               FK to ObservationGeometry.id
provider                   none / monocular / stereo / tof / rgbd / manual
frame_id                  source-frame/provenance identifier
response_id               provider response identifier
prompt_version            provider prompt/schema version
backend_version           provider backend version
mode                       relative / metric
relation_to_camera        nearer / middle / farther / unknown
                           (logical storage may normalize near/mid/far)
value_m                   optional metric distance (future calibrated mode)
uncertainty_m             optional metric uncertainty (future calibrated mode)
relative_score            optional ordinal score within one response
valid_fraction            fraction of bbox with valid depth [0, 1]
coordinate_frame          camera frame identifier, if metric
calibration_ref           calibration/version identifier, if metric
model_ref                 provider model/version, if estimated
quality                   good / degraded / invalid
```

For an object bbox, the robust statistic should normally be a percentile or
trimmed median over valid pixels, not the nearest pixel. The aggregator must
record `valid_fraction` because reflective, transparent, or textureless
surfaces can produce bad depth.

A monocular provider may write:

```text
mode = relative
relation_to_camera = nearer
relative_score = 0.81
value_m = NULL
```

It must not write an invented `value_m` merely because the model returns a
floating-point map.

### 4.3 Object size evidence

Size evidence is an estimate, never automatically a fact. It can be learned
from a calibrated scene, supplied as a class prior, or entered manually.

```text
SizeEvidence
------------
id                         stable evidence id
observation_id            optional FK to observations.id
label_or_object_kind      semantic kind used for the estimate
source                     metric / class_prior / manual / unknown
length_min_m              optional lower bound
length_max_m              optional upper bound
width_min_m               optional lower bound
width_max_m               optional upper bound
height_min_m              optional lower bound
height_max_m              optional upper bound
orientation               unknown / horizontal / vertical / estimated pose
uncertainty                low / medium / high
calibration_ref           optional calibration/version identifier
model_ref                 optional estimator/model version
```

For a known household class, an interval prior is preferable to a single
number:

```text
keys:  length 0.05..0.08 m, source=class_prior, uncertainty=high
remote: length 0.15..0.25 m, source=class_prior, uncertainty=high
```

A class prior can support a hypothesis, but must not by itself establish
containment.

### 4.4 Container affordance evidence

A container needs an estimated usable interior, not just an external bounding
box. This is often unknown from a single view.

```text
ContainerAffordance
-------------------
geometry_id               FK to the container geometry sample
kind                      box / drawer / bag / cabinet / tray / cover / unknown
opening_state              open / closed / unclear / not_applicable
opening_direction          toward_camera / away / upward / side / unknown
interior_length_min_m      optional lower bound
interior_length_max_m      optional upper bound
interior_width_min_m       optional lower bound
interior_width_max_m       optional upper bound
interior_height_min_m      optional lower bound
interior_height_max_m      optional upper bound
usable_volume_min_m3       optional lower bound
usable_volume_max_m3       optional upper bound
source                     visual_estimate / class_prior / manual / unknown
uncertainty                low / medium / high
```

External dimensions must not be silently reused as interior dimensions. If the
model only sees a closed box, `opening_state=closed` and interior dimensions
may remain unknown; the box can still be a cover candidate.

## 5. Event records and lifecycle boundaries

The resident ingest mirrors `appeared` and `disappeared` in JSONL, while G1
makes these facts authoritative and queryable in SQLite's `observation_events`.
The JSONL file remains an operational/debug output, not state. Event rows retain
source and noticed timestamps, session/reason metadata, and an optional
immutable event-boundary geometry reference.

```text
ObservationEvent (implemented G1 fields)
-----------------------------------------
id                         primary key
observation_id            FK to observations.id
camera_id                 camera identity
zone                       observation zone
label                     observation label
event_type                appeared / disappeared
occurred_at               source event timestamp
noticed_at                ingest/observation timestamp
source                    local_detector / frigate / manual
session_id                optional ingest session
reason                    lifecycle reason
hits                      hit count at lifecycle boundary
seen_for_s                 optional residence duration
geometry_id               optional immutable geometry evidence
```

`covered` and `contained` should not be written as raw detector events. They are
derived relations and belong in the inference layer. A future event table may
record the accepted conclusion, but the original appeared/disappeared facts
must remain available.

The lifecycle event vocabulary currently implemented is `appeared` and
`disappeared`; `observed` and `moved` remain reserved design vocabulary. The
first useful lifecycle pattern is:

```text
A appeared at t0
A observed through t1
B appeared or persisted at t2
A disappeared at t3
B persisted after t3
```

The order is evidence, not proof. A delayed webhook or a late frame must retain
its source timestamp and ingestion timestamp if those differ.

## 6. Derived relations and hypotheses

### 6.1 Relation types

The initial vocabulary should be deliberately cautious:

```text
possibly_occluded_by     B blocks the view of A
possibly_under           A may be underneath B
possibly_contained_in    A may be inside B
near                     A and B are spatially close
moved_with               A's position may follow B after containment
not_supported            a candidate was evaluated but evidence conflicts
```

`possibly_occluded_by` and `possibly_contained_in` must not be collapsed:

- occlusion only claims a visibility explanation;
- containment additionally requires container semantics and compatible usable
  space;
- under-cover is a useful middle result when the container interior is
  unknown.

### 6.2 Inference record

```text
Inference
---------
id                         primary key
camera_id                 camera identity
target_observation_id     object that disappeared or is being located
container_observation_id  optional covering/container observation
relation                   relation vocabulary above
status                    candidate / supported / rejected / expired
confidence                numeric score [0, 1]
created_at                when this inference was computed
valid_from                earliest supported time
valid_until               optional expiry time
rule_version              deterministic rule/model version
explanation               short human-readable explanation
```

The evidence links should be normalized rather than embedded only in prose:

```text
InferenceEvidence
-----------------
inference_id
kind                       temporal / overlap / depth / size / affordance /
                           persistence / semantic
source_id                  id of the source evidence record
contribution               supporting / conflicting / unknown
weight                     optional rule contribution
```

An inference should be able to say:

```text
relation = possibly_contained_in
status = candidate
confidence = 0.64
explanation = "keys disappeared 2.4 s after a box appeared over the same desk area; depth ordering is compatible; box interior size is unknown"
```

This is intentionally not the answer `keys are in the box`.

## 7. Evidence and scoring rules

The first rule engine should be deterministic and explainable. It should
produce a candidate only when the target and candidate are in the same camera
and their event times fall within a configurable window.

### 7.1 Hard gates

Reject or leave `unknown` when:

- camera identities differ and no cross-camera calibration exists;
- the candidate appears long after the target disappeared;
- the candidate cannot be spatially related at all;
- the alleged container is known to be too small under a reliable metric
  measurement;
- timestamps are invalid or have an unresolved clock domain;
- the target was already observed again after the alleged disappearance.

A missing depth or size measurement is not a hard rejection.

### 7.2 Supporting evidence

The initial score can combine interpretable components:

```text
temporal_proximity       0.20
projected_overlap        0.20
depth_compatibility      0.20
size_compatibility       0.15
container_persistence    0.15
semantic_affordance      0.10
```

These weights are a starting point for experiments, not a public truth. Each
component must carry one of `supporting`, `conflicting`, or `unknown`, so the
explanation can distinguish "no evidence" from "evidence against".

Suggested interpretation:

- high overlap + no depth + unknown interior → `possibly_under` or
  `possibly_occluded_by`, not `possibly_contained_in`;
- overlap + compatible depth + known container + size fit →
  `possibly_contained_in` candidate;
- short-lived person/hand overlap → transient occlusion candidate, never an
  automatic container relation;
- target reappears after the candidate moves → expire or reject the relation.

### 7.3 Fit is interval logic

For a required object extent `[a_min, a_max]` and available interior extent
`[b_min, b_max]`:

```text
fits          when a_max <= b_min is supported with adequate confidence
not_fits      when a_min > b_max is supported with adequate confidence
a_unknown     when the intervals overlap or either side lacks a usable bound
```

The exact orientation and opening constraints are part of the affordance
record. A long object may fit diagonally even when its axis-aligned length does
not; the first implementation should return `unknown` rather than pretend to
solve arbitrary 3-D packing.

## 8. Query semantics

The query layer should answer from original observations first and add an
inference only when one exists. Example response shape:

```text
Keys were last directly seen on the desk at 14:32:10.
A box appeared over the same area 2.4 seconds later and remained there.
The keys may be inside or underneath the box (medium confidence); the box's
usable interior was not measured.
```

If only a cover relation is supported:

```text
Keys were last seen on the desk. A book then covered that area, so the keys
may be underneath it. This is not evidence that they are inside the book.
```

If no candidate exists, do not invent a location. Report the last direct sighting
and that no supported covering/container relation was found.

## 9. Implementation boundaries

### Phase G1: persist geometry facts — implemented

- SQLite stores first/last and immutable event-boundary geometry evidence in
  `observation_geometry`.
- SQLite stores authoritative `appeared`/`disappeared` rows in
  `observation_events`; `events.jsonl` remains an operational/debug mirror.
- Optional depth rows attach to geometry in `depth_evidence`.
- No tracker or per-frame SQLite table is used.

### Phase G2: derive conservative 2-D candidates — implemented

- `item-query candidates` matches a `missed_gap` disappearance with a nearby,
  persistent appearance in one camera, zone, and ingest session.
- It requires valid same-frame geometry compatibility, projected overlap, timing,
  and persistence; later target reappearance and conflicting depth reject it.
- It emits only `possibly_occluded_by` or `possibly_under` hypotheses with
  evidence contributions, source geometry ids, and a deterministic heuristic
  ordering score. The score is not a probability.
- Deterministic tests cover synthetic lifecycle sequences and read-side store
  integration.

### Phase G3: optional relative-depth providers — implemented

- `item-ingest` defines an optional provider interface and an OpenAI-compatible
  ordinal VLM provider behind the `relative-depth` feature.
- Provider output is provenance-bearing (`frame_id`, `response_id`, prompt and
  backend versions, model reference, quality) and may support or conflict with
  a G2 candidate only when same-frame/provider-response evidence is comparable.
- The current provider never emits metric distance, calibration, or a physical
  containment decision. Missing or incomparable output remains `unknown`.

### Phase G4: add size and affordance reasoning — implemented (read side)

- `item-query --priors <file>` loads a TOML file of object size intervals
  and per-camera/zone container records (role, opening state, usable interior
  intervals); see `crates/item-query/priors.example.toml`. Built-in class
  priors cover the default VLM targets and the G2 cover vocabulary. Nothing is
  persisted: priors are configuration, and each candidate carries a
  `priors_ref` (built-in version plus file path and content hash) so a result
  can be traced to the priors that produced it.
- Fit is the §7.3 interval check: `fits` when the object's largest extents
  fit the interior's smallest in some axis-aligned orientation; `not_fits`
  only when an extent is certainly longer than the interior's space diagonal;
  otherwise `unknown`. No packing is attempted.
- `possibly_contained_in` requires all of: a priors-file container entry
  (built-in class priors never promote), `opening_state = open`, a fit, a
  cover reaching over at least 50% of the target's last box, unambiguous
  identity, and every G2 gate. A size conflict or closed/unclear opening keeps
  the G2 relation and records the conflicting/unknown evidence; a `cover` role
  never contains; a person or hand only occludes.
- Not implemented: opening size vs. object cross-section, visual estimation
  of opening state or interior, metric size from calibrated depth, and
  persisting accepted inferences (§6.2).

### Phase G5: query and UI evidence cards — future

- Display the last direct sighting, cover evidence, timestamps, relation,
  heuristic score, explanation, snapshots, and measurement provenance.
- Never display a probabilistic inference as a direct observation or claim that
  a VLM has validated containment.

## 10. Verification scenarios

The first acceptance set should use a fixed camera and detectable stand-in
objects with known ground truth:

1. target beside a box;
2. target underneath a box;
3. target placed inside an open box;
4. target leaves the frame with no container;
5. a hand/person crosses the target briefly;
6. a container is too small for the target;
7. a box moves after the target disappears;
8. depth is unavailable or invalid;
9. target reappears after an apparent covering event.

For every scenario, retain the raw event sequence and inspect:

- whether the candidate was generated;
- which evidence was supporting, conflicting, or unknown;
- whether the relation type was appropriately cautious;
- whether the target's direct last-seen answer remained intact;
- whether the relation expired when contradictory evidence arrived.

The key success criterion is not perfect containment classification or a claimed
VLM accuracy number. G2/G3 should reduce unsupported visibility claims while
preserving useful direct last-seen answers and making supporting, conflicting,
and unknown evidence explicit. Any future containment result must remain a
qualified hypothesis unless independently verified.

## 11. Explicit non-goals

This document does not commit the project to:

- centimetre-accurate monocular reconstruction;
- automatic object re-identification across cameras;
- arbitrary 3-D mesh reconstruction or packing simulation;
- treating VLM-generated dimensions as ground truth;
- inferring `inside` from 2-D overlap alone;
- storing raw per-frame detections indefinitely;
- replacing the existing observation log with a black-box tracker.
