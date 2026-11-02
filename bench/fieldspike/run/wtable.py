import json,sys
rows=[]
for s in sys.argv[1:]:
    d=json.load(open(f'results/{s}/analysis.json'))
    for users,v in d['steps'].items():
        tot=v.get('bytes_indexer',0)+v.get('bytes_save',0)
        g=lambda k: v.get(k,0)
        print(f"| `{s}` | {users} | {v['ok_ops']} | {tot/1000:.1f} | {g('bytes_indexer')/1000:.1f} | {g('bytes_save')/1000:.1f} | {g('records_indexer')+g('records_save'):.0f} | {g('content_records'):.2f} ({g('content_new'):.2f} new) | {g('content_bytes')/1000:.2f} | {g('changed_field_bytes'):.0f} | {g('b_decl_not_written'):.0f} | {g('b_all_not_written'):.0f} | {100*g('b_all_not_written')/tot:.2f} | {g('a_slot_bytes'):.0f} | {100*g('a_slot_bytes')/tot:.2f} |")
    print('  fields:', json.dumps(d['field_changes']))
