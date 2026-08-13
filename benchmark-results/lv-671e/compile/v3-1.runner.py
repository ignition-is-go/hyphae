#!/usr/bin/env python3
"""Measure a serial clean build of a revision plus its semantic evidence adapter."""
import argparse, hashlib, json, os, pathlib, resource, shutil, subprocess, tempfile, time
p=argparse.ArgumentParser()
p.add_argument('--checkout', required=True, help='git repository used for archive/revision lookup')
p.add_argument('--revision', required=True)
p.add_argument('--adapter', required=True)
p.add_argument('--target-dir', required=True)
p.add_argument('--output', required=True)
p.add_argument('--cache-root', default='/home/trevor/.cache/hyphae-evidence')
a=p.parse_args(); home=pathlib.Path('/home/trevor').resolve()
def under_home(value, label):
 path=pathlib.Path(value).resolve()
 if not path.is_relative_to(home): p.error(f'{label} must be under {home}: {path}')
 return path
checkout=under_home(a.checkout,'checkout'); adapter=under_home(a.adapter,'adapter'); target=under_home(a.target_dir,'target-dir'); out=under_home(a.output,'output'); cache=under_home(a.cache_root,'cache-root')
if target.exists(): p.error(f'target-dir must not exist (clean build required): {target}')
out.parent.mkdir(parents=True,exist_ok=True); cache.mkdir(parents=True,exist_ok=True)
frozen_adapter=out.with_suffix('.adapter.rs'); frozen_runner=out.with_suffix('.runner.py'); shutil.copy2(adapter,frozen_adapter); shutil.copy2(pathlib.Path(__file__).resolve(),frozen_runner); adapter=frozen_adapter
commit=subprocess.check_output(['git','rev-parse',a.revision],cwd=checkout,text=True).strip()
run=pathlib.Path(tempfile.mkdtemp(prefix='compile-resources.',dir=cache))
try:
 archive=subprocess.Popen(['git','archive',commit],cwd=checkout,stdout=subprocess.PIPE)
 extract=subprocess.run(['tar','-x','-C',str(run)],stdin=archive.stdout,check=True); archive.stdout.close(); archive_rc=archive.wait()
 if archive_rc: raise subprocess.CalledProcessError(archive_rc,archive.args)
 example=run/'hyphae/examples/map_query_allocation_profile.rs'; example.parent.mkdir(parents=True,exist_ok=True); shutil.copy2(adapter,example)
 target.mkdir(parents=True,exist_ok=False)
 env=os.environ.copy(); env.update(CARGO_BUILD_JOBS='1',CARGO_TARGET_DIR=str(target))
 cmd=['cargo','build','--locked','--offline','--release','-p','hyphae','--example','map_query_allocation_profile']
 before_usage=resource.getrusage(resource.RUSAGE_CHILDREN); t0=time.monotonic_ns(); cp=subprocess.run(cmd,cwd=run,env=env,text=True,capture_output=True); wall=(time.monotonic_ns()-t0)/1e9; usage=resource.getrusage(resource.RUSAGE_CHILDREN)
 out.with_suffix('.stdout.txt').write_text(cp.stdout); out.with_suffix('.stderr.txt').write_text(cp.stderr)
 artifact=target/'release/examples/map_query_allocation_profile'; artifact=artifact if artifact.is_file() else None
 toolchain={'rustc':subprocess.check_output(['rustc','-vV'],text=True).strip(),'cargo':subprocess.check_output(['cargo','-V'],text=True).strip(),'uname':subprocess.check_output(['uname','-srmo'],text=True).strip(),'cpu':subprocess.check_output(['lscpu'],text=True).strip()}
 normalized_env={k:env.get(k,'') for k in ('CARGO_BUILD_JOBS','CARGO_TARGET_DIR','RUSTFLAGS','RUSTC_WRAPPER','CARGO_INCREMENTAL')}
 data={'schema':'hyphae.compile-resources/2','command':cmd,'checkout':str(checkout),'revision':a.revision,'commit':commit,'adapter':str(adapter),'adapter_sha256':hashlib.sha256(adapter.read_bytes()).hexdigest(),'runner_sha256':hashlib.sha256(frozen_runner.read_bytes()).hexdigest(),'environment':normalized_env,'toolchain_hardware':toolchain,'target_dir':str(target),'exit_code':cp.returncode,'wall_s':wall,'user_s':usage.ru_utime-before_usage.ru_utime,'sys_s':usage.ru_stime-before_usage.ru_stime,'maxrss_kib':usage.ru_maxrss,'artifact':str(artifact) if artifact else None,'artifact_bytes':artifact.stat().st_size if artifact else None,'artifact_sha256':hashlib.sha256(artifact.read_bytes()).hexdigest() if artifact else None}
 out.write_text(json.dumps(data,indent=2)+'\n'); raise SystemExit(cp.returncode)
finally: shutil.rmtree(run,ignore_errors=True)
