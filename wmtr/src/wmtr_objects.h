/* Host-only object provenance. Each exported +1 or virtual-pool retain owns one registry reference.
 * Validation and temporary retains share a lock with removing the last owner: a worker never races a
 * pool drain/release into objc_msgSend. Unknown handles are rejected before any Objective-C call.
 * The wire still uses DXMT's object handles; this is not a hostile-guest sandbox/type system. */
static int object_checks(struct wmtr_host *h) {
#ifdef WMTR_TESTING
  return h->strict_objects;
#else
  return 1;
#endif
}
static struct object_entry *object_find(struct wmtr_host *h, uint64_t value, int insert) {
  uint32_t start = (uint32_t)((value >> 4) * 0x9e3779b1u) & (WMTR_HANDLE_SLOTS - 1);
  struct object_entry *empty = NULL;
  for (uint32_t n = 0; n < WMTR_HANDLE_SLOTS; n++) {
    struct object_entry *e = &h->objects[(start + n) & (WMTR_HANDLE_SLOTS - 1)];
    if (e->value == value) return e;
    if (!e->value) return insert ? (empty ? empty : e) : NULL;
    if (e->value == UINT64_MAX && !empty) empty = e;
  }
  return insert ? empty : NULL;
}
static void object_drop(struct wmtr_host *h, uint64_t obj, int pool) {
  if (!obj || !object_checks(h)) return;
  pthread_mutex_lock(&h->object_lock);
  struct object_entry *e = object_find(h, obj, 0);
  if (pool && e && e->pool_refs) e->pool_refs--;
  if (e && e->refs && !--e->refs) e->value = UINT64_MAX;
  pthread_mutex_unlock(&h->object_lock);
}
struct object_walk { struct wmtr_host *h; uint8_t *base; uint32_t size; int mode; uint16_t call; };
enum { OBJECT_VALIDATE, OBJECT_LEASE, OBJECT_UNLEASE, OBJECT_EXPORT };
static int object_field(struct object_walk *w, obj_handle_t *field) {
  uint64_t value = *field;
  if (!value) return 1;
  if (w->mode == OBJECT_UNLEASE) {
    if (!(w->call == 104 && field == &((struct unixcall_mtlsharedevent_setevent *)w->base)->shared_event_listener) &&
        w->call != 110 && w->call != 109) [(id)value release];
    return 1;
  }
  struct object_entry *e = object_find(w->h, value, w->mode == OBJECT_EXPORT);
  if (w->mode == OBJECT_EXPORT) {
    if (!e) return 0;
    if (e->value != value) { e->value = value; e->refs = e->pool_refs = 0; e->plain = w->call == 108; }
    e->refs++;
    switch (w->call) {
    case 2: case 7: case 11: case 28: case 30: case 31: case 32: case 53: case 60: case 62:
    case 66: case 67: case 71: case 89: case 90: case 91: case 129: e->pool_refs++; break;
    default: break;
    }
    return 1;
  }
  if (!e || !e->refs) {
    LOG(0, "call %u: unknown/released object handle %llx; refusing call\n", w->call, (unsigned long long)value);
    return 0;
  }
  if (w->mode == OBJECT_VALIDATE && w->call == 1 && e->refs == e->pool_refs) {
    LOG(0, "call 1: release of borrowed object %llx without retain; refusing call\n", (unsigned long long)value);
    return 0;
  }
  if (w->mode == OBJECT_LEASE && !e->plain) [(id)value retain];
  return 1;
}
#define WMTR_OBJECT_STRUCTS
#include "wmtr_objects.inc"
#undef WMTR_OBJECT_STRUCTS
static int object_range(struct object_walk *w, const void *ptr, uint64_t count, uint64_t stride) {
  uintptr_t p = (uintptr_t)ptr, b = (uintptr_t)w->base;
  return !count || (ptr && p >= b && p - b <= w->size && count <= (w->size - (p - b)) / stride);
}
#define OBJECT_INFO(T, P) do { struct T *object_info_ptr = (void *)(P); if (!object_range(w, object_info_ptr, 1, sizeof *object_info_ptr) || !objects_##T(w, object_info_ptr, 0)) return 0; } while (0)
#define PIPELINE(T) do { struct T *i = (void *)((struct unixcall_mtldevice_newcomputepso *)payload)->info.ptr; \
  OBJECT_INFO(T, i); obj_handle_t *a = (void *)i->binary_archives_for_lookup.ptr; \
  if (!object_range(w, a, i->num_binary_archives_for_lookup, sizeof *a)) return 0; \
  for (uint32_t n = 0; n < i->num_binary_archives_for_lookup; n++) if (!object_field(w, &a[n])) return 0; } while (0)
static int objects_walk(struct object_walk *w, uint8_t *payload, int output) {
  uint16_t call = w->call;
  if (call == 0 || call == 1) {
    if (output) return 1;
    uint64_t value = *(uint64_t *)payload;
    if (is_vpool(w->h, value))
      return w->mode != OBJECT_VALIDATE || ((struct vpool *)(uintptr_t)value)->used;
    return object_field(w, (void *)payload);
  }
  if (!output) switch (call) {
  case 29: PIPELINE(WMTComputePipelineInfo); break;
  case 34: PIPELINE(WMTRenderPipelineInfo); break;
  case 35: PIPELINE(WMTMeshRenderPipelineInfo); break;
  case 131: PIPELINE(WMTTileRenderPipelineInfo); break;
  case 32: OBJECT_INFO(WMTRenderPassInfo, (void *)(uintptr_t)((struct unixcall_generic_obj_uint64_obj_ret *)payload)->arg); break;
  case 54: OBJECT_INFO(WMTCaptureInfo, ((struct unixcall_mtlcapturemanager_startcapture *)payload)->info.ptr); break;
  case 70: OBJECT_INFO(WMTLayerProps, ((struct unixcall_generic_obj_constptr_noret *)payload)->arg.ptr); break;
  case 129: {
    struct unixcall_mtlcommandbuffer_blitcommandencoderwithsamplebuffers *p = (void *)payload;
    struct WMTSampleBufferAttachmentInfo *a = p->attachments.ptr;
    if (!object_range(w, a, p->num_attachments, sizeof *a)) return 0;
    for (uint64_t i = 0; i < p->num_attachments; i++) if (!objects_WMTSampleBufferAttachmentInfo(w, &a[i], 0)) return 0;
    break;
  }
  case 133: case 134: {
    struct unixcall_mtlresidencyset_addallocations *p = (void *)payload;
    obj_handle_t *a = (void *)p->allocations.ptr;
    if (!object_range(w, a, p->count, sizeof *a)) return 0;
    for (uint64_t i = 0; i < p->count; i++) if (!object_field(w, &a[i])) return 0;
    break;
  }
  case 36: case 37: case 38: {
    struct wmtcmd_base *c = (void *)((struct unixcall_generic_obj_cmd_noret *)payload)->cmd_head.ptr;
    for (uint32_t n = 0; c; c = c->next.ptr, n++) {
      if (n > w->size / sizeof *c || !object_range(w, c, 1, sizeof *c)) return 0;
      switch (call - 36) {
#define CMDS(K) switch (c->type) {
#define CMD(kind, value, type) case value: OBJECT_INFO(type, c); break;
      case 0: CMDS(BLIT)
#define WMTR_COMMAND_BLIT
#include "wmtr_object_cmds.inc"
#undef WMTR_COMMAND_BLIT
        default: return 0; } break;
      case 1: CMDS(COMPUTE)
#define WMTR_COMMAND_COMPUTE
#include "wmtr_object_cmds.inc"
#undef WMTR_COMMAND_COMPUTE
        default: return 0; } break;
      case 2: CMDS(RENDER)
#define WMTR_COMMAND_RENDER
#include "wmtr_object_cmds.inc"
#undef WMTR_COMMAND_RENDER
        default: return 0; } break;
#undef CMD
#undef CMDS
      }
    }
    break;
  }
  default: break;
  }
  if (call == 114) return !output || object_field(w, &((struct unixcall_generic_obj_uint64_obj_ret *)payload)->ret);
  if (call == 71 && output) {
    struct WMTLayerProps *p = ((struct unixcall_generic_obj_ptr_noret *)payload)->arg.ptr;
    if (!object_range(w, p, 1, sizeof *p) || !object_field(w, &p->device)) return 0;
  }
  if (call == 91 && output) {
    struct unixcall_enumerate *p = (void *)payload;
    obj_handle_t *a = p->buffer.ptr;
    if (p->ret_read > p->buffer_size || !object_range(w, a, p->ret_read, sizeof *a)) return 0;
    for (uint64_t i = 0; i < p->ret_read; i++) if (!object_field(w, &a[i])) return 0;
  }
  switch (call) {
#define WMTR_OBJECT_CALLS
#include "wmtr_objects.inc"
#undef WMTR_OBJECT_CALLS
  default: return 1;
  }
}
#undef PIPELINE
#undef OBJECT_INFO
