import json

with open('run/pc1_stats_live.json', encoding='utf-8', errors='replace') as f:
    d = json.loads(f.read())

content = d.get('content', '')
if not content:
    print('no content:', d)
else:
    s = json.loads(content)
    fields = ['running','sent_frames','fps_actual','gpu_ms','gpu_dispatch_ms',
              'gpu_dispatch_samples','pc1_seg_avg_ms','pc1_seg_p99_ms',
              'pc1_seg_max_ms','send_errors','cpu_pct','gpu_convert']
    for k in fields:
        print('%s: %s' % (k, s.get(k, 'N/A')))
