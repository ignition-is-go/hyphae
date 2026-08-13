#!/usr/bin/env python3
"""Build one frozen MapQuery evidence binary, then run one fresh process per matrix cell."""
import argparse,csv,hashlib,json,os,pathlib,shutil,subprocess,sys
SIZES=(1,10,100,1000,10000); SCENARIOS=("projection_region","two_join_region","repeated_relation_four_join","rekey_between_joins"); PHASES=("setup","build","materialize","single_updates","batch","teardown")
def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()
def main():
 p=argparse.ArgumentParser(); p.add_argument('--revision',required=True); p.add_argument('--adapter',choices=('v3','candidate'),required=True); p.add_argument('--output-root',required=True); p.add_argument('--small',action='store_true'); a=p.parse_args()
 home=pathlib.Path('/home/trevor').resolve(); repo=pathlib.Path(subprocess.check_output(['git','rev-parse','--show-toplevel'],text=True).strip()).resolve(); out=pathlib.Path(a.output_root).resolve()
 if not out.is_relative_to(home): p.error(f'output must be under {home}')
 if out.exists() and any(out.iterdir()): p.error(f'output must be empty: {out}')
 out.mkdir(parents=True,exist_ok=True); commit=subprocess.check_output(['git','rev-parse',a.revision],cwd=repo,text=True).strip(); inputs=out/'inputs'; inputs.mkdir()
 adapter_src=repo/'tools'/('map_query_allocation_profile_v3.rs' if a.adapter=='v3' else 'map_query_allocation_profile.rs'); adapter=inputs/adapter_src.name
 for src,dst in ((adapter_src,adapter),(repo/'tools/bench-map-query-allocations.sh',inputs/'bench-map-query-allocations.sh'),(pathlib.Path(__file__).resolve(),inputs/'run-map-query-evidence.py')): shutil.copy2(src,dst)
 checkout=out/'run-checkout'; checkout.mkdir(); archive=subprocess.Popen(['git','archive',commit],cwd=repo,stdout=subprocess.PIPE); subprocess.run(['tar','-x','-C',str(checkout)],stdin=archive.stdout,check=True); archive.stdout.close(); arc_rc=archive.wait()
 if arc_rc: raise subprocess.CalledProcessError(arc_rc,archive.args)
 example=checkout/'hyphae/examples/map_query_allocation_profile.rs'; example.parent.mkdir(parents=True,exist_ok=True); shutil.copy2(adapter,example)
 target=out/'target'; env=os.environ.copy(); env.update(CARGO_BUILD_JOBS='1',CARGO_TARGET_DIR=str(target),HYPHAE_BENCH_REVISION=commit)
 build_cmd=['cargo','build','--locked','--offline','--release','-p','hyphae','--example','map_query_allocation_profile']; build=subprocess.run(build_cmd,cwd=checkout,env=env,text=True,capture_output=True); (out/'build.stdout.txt').write_text(build.stdout); (out/'build.stderr.txt').write_text(build.stderr)
 if build.returncode: raise SystemExit(f'evidence build failed: {build.returncode}')
 binary=target/'release/examples/map_query_allocation_profile'
 if not binary.is_file(): raise SystemExit(f'missing evidence binary: {binary}')
 binary_sha=sha(binary); sizes=(1,) if a.small else SIZES
 manifest={'schema':'hyphae.map-query-evidence/3','requested_revision':a.revision,'commit':commit,'adapter_kind':a.adapter,'sizes':sizes,'scenarios':SCENARIOS,'build_command':build_cmd,'binary':str(binary.relative_to(out)),'binary_sha256':binary_sha,'inputs':{x.name:sha(x) for x in inputs.iterdir()}}
 (out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n'); combined=out/'results.csv'; cells=[]; wrote_header=False
 for scenario in SCENARIOS:
  for rows in sizes:
   for batch in sizes:
    name=f'{scenario}-n{rows}-b{batch}'; cell=out/name; cell.mkdir(); cell_env=os.environ.copy(); cell_env.update(HYPHAE_EVIDENCE_ROWS=str(rows),HYPHAE_EVIDENCE_BATCH=str(batch),HYPHAE_EVIDENCE_SCENARIO=scenario)
    cp=subprocess.run([str(binary)],env=cell_env,text=True,capture_output=True); (cell/'results.txt').write_text(cp.stdout); (cell/'stderr.txt').write_text(cp.stderr)
    (cell/'environment.json').write_text(json.dumps({'commit':commit,'binary_sha256':binary_sha,'scenario':scenario,'rows':rows,'batch_size':batch,'single_updates':int(cell_env.get('HYPHAE_EVIDENCE_SINGLE_UPDATES','100')),'exit_code':cp.returncode},sort_keys=True,indent=2)+'\n')
    if cp.returncode: raise SystemExit(f'{name} failed: {cp.returncode}')
    raw=[line.removeprefix('MAP_QUERY_ALLOCATION_CSV ') for line in cp.stdout.splitlines() if line.startswith('MAP_QUERY_ALLOCATION_CSV ')]; parsed=list(csv.DictReader(raw)); phases=[r['phase'] for r in parsed]
    if len(parsed)!=6 or sorted(phases)!=sorted(PHASES) or len(set(phases))!=6: raise SystemExit(f'{name}: invalid phases {phases}')
    if any(r['revision']!=commit or r['scenario']!=scenario or int(r['rows'])!=rows or int(r['batch_size'])!=batch for r in parsed): raise SystemExit(f'{name}: identity mismatch')
    if int(parsed[-1]['live_bytes_after'])!=int(parsed[0]['live_bytes_before']): raise SystemExit(f'{name}: teardown baseline mismatch')
    with combined.open('a') as f:
     for i,line in enumerate(raw):
      if i==0 and wrote_header: continue
      f.write(line+'\n')
    wrote_header=True; cells.append({'name':name,'status':'ok','results_sha256':sha(cell/'results.txt'),'rows':len(parsed)})
 (out/'cells.json').write_text(json.dumps(cells,indent=2)+'\n'); (out/'COMPLETE').write_text(hashlib.sha256(combined.read_bytes()).hexdigest()+'  results.csv\n'); return 0
if __name__=='__main__': sys.exit(main())
