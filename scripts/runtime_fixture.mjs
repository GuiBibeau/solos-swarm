// Separate integration controller. Synthetic issuer/provider fixtures never certify production MFA or live inference.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import https from 'node:https';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
const [mode, root] = process.argv.slice(2);
if (!['serve','drive','inspect'].includes(mode) || !root || !path.isAbsolute(root)) throw new Error('usage: runtime_fixture.mjs serve|drive <absolute-owned-fixture-directory>');
const readJson = (name) => JSON.parse(fs.readFileSync(path.join(root,name),'utf8'));
const rawPackets = new WeakMap();
const write = (name,value) => fs.writeFileSync(path.join(root,name),rawPackets.get(value)??JSON.stringify(value,null,2),{mode:0o600});
// Keep serde_json::Value numeric spellings intact for the signed state checkpoint.
const rawMember = (packet,member) => {
  const source=rawPackets.get(packet)?.toString('utf8');if(!source)throw new Error('authoritative packet bytes missing');
  let i=0;const white=()=>{while(/\s/.test(source[i]??'x'))i++;};
  const quoted=()=>{if(source[i++]!=='"')throw new Error('invalid raw JSON string');while(i<source.length){if(source[i]==='\\'){i+=2;continue;}if(source[i++]==='"')return;}throw new Error('unterminated raw JSON string');};
  const value=()=>{if(source[i]==='"'){quoted();return;}if(source[i]==='{'||source[i]==='['){const stack=[];do{const c=source[i];if(c==='"'){quoted();continue;}i++;if(c==='{'||c==='[')stack.push(c);else if(c==='}'||c===']'){const opening=stack.pop();if((c==='}'&&opening!=='{')||(c===']'&&opening!=='['))throw new Error('raw JSON nesting mismatch');}}while(stack.length&&i<source.length);if(stack.length)throw new Error('unterminated raw JSON container');return;}while(i<source.length&&!/[\s,}\]]/.test(source[i]))i++;};
  white();if(source[i++]!=='{')throw new Error('authoritative packet must be an object');
  for(;;){white();if(source[i]==='}')break;const keyStart=i;quoted();const key=JSON.parse(source.slice(keyStart,i));white();if(source[i++]!==':')throw new Error('invalid raw JSON member');white();const start=i;value();if(key===member)return Buffer.from(source.slice(start,i));white();if(source[i]===','){i++;continue;}if(source[i]==='}')break;throw new Error('invalid raw JSON separator');}
  throw new Error(`authoritative packet has no ${member}`);
};
const rawPath = (packet,keys) => {let value=packet;let bytes=rawPackets.get(packet);for(const key of keys){bytes=rawMember(value,key);value=value[key];if(value&&typeof value==='object')rawPackets.set(value,bytes);}return bytes;};
const oidc = readJson('issuer/oidc.json');
const jwks = readJson('issuer/jwks.json');
const manifest = readJson('fixture-manifest.json');
const bounded = (request, callback) => {
  let size=0; const chunks=[];
  request.on('data',(chunk)=>{size+=chunk.length;if(size>1048576) request.destroy();else chunks.push(chunk);});
  request.on('end',()=>{try{callback(JSON.parse(Buffer.concat(chunks).toString('utf8')));}catch{request.destroy();}});
  request.setTimeout(60000,()=>request.destroy());
};
const jsonResponse = (response,value,status=200) => {response.writeHead(status,{'Content-Type':'application/json','Cache-Control':'max-age=120'});response.end(JSON.stringify(value));};
const text = (value) => typeof value==='string'?value:Array.isArray(value)?value.map((part)=>part.text??'').join('\n'):JSON.stringify(value);
const unequal = (records) => records.filter((row)=>row.source_a!==row.source_b);
if (mode==='serve') {
  const issuer = https.createServer({cert:fs.readFileSync(path.join(root,'issuer/tls.pem')),key:fs.readFileSync(path.join(root,'issuer/tls.key.pem')),minVersion:'TLSv1.3'},(request,response)=>{
    if(request.url==='/.well-known/openid-configuration')jsonResponse(response,{issuer:oidc.issuer,jwks_uri:`${oidc.issuer}/keys`,id_token_signing_alg_values_supported:['EdDSA']});
    else if(request.url==='/keys')jsonResponse(response,jwks);
    else jsonResponse(response,{error:'no fixture authority endpoint'},404);
  });
  const contacts=[];
  const provider=http.createServer({maxHeaderSize:8192},(request,response)=>{
    if(request.method!=='POST'||request.url!=='/chat/completions'){jsonResponse(response,{error:'unknown fixture provider path'},404);return;}
    bounded(request,(body)=>{
      const messages=body.messages??[];
      const prompt=messages.map((message)=>text(message.content)).join('\n');
      const task=prompt.match(/Assigned Task: (task-\d{2})/)?.[1];
      if(!task){jsonResponse(response,{error:'fixture provider requires actual assigned Task context'},400);return;}
      const index=Number(task.slice(-2));
      const caseData=readJson('issuer/public-case.json');
      const calls=messages.flatMap((message)=>message.tool_calls??[]);
      const results=new Map(messages.filter((message)=>message.role==='tool').map((message)=>{
        let result;try{result=JSON.parse(text(message.content));}catch{result={};}
        return [message.tool_call_id,result];
      }));
      const resultFor = (name) => {
        const call=calls.findLast((call)=>call.function?.name===name);return call?results.get(call.id):undefined;
      };
      let name;let argumentsValue;
      if(!calls.some((call)=>call.function?.name==='submit_finding')){
        name='submit_finding';argumentsValue={body:{inconsistencies:unequal(caseData.tasks[index].input.records)}};
      }else if(index===9 && !calls.some((call)=>call.function?.name==='submit_attempt')){
        const findings=resultFor('read_findings')?.findings??[];
        if(findings.length<10){name='read_findings';argumentsValue={};}
        else if((resultFor('task_status')?.tasks??[]).filter((task)=>/^task-0[0-8]$/.test(task.id)&&task.state==='completed').length!==9){name='task_status';argumentsValue={limit:128};}
        else{name='submit_attempt';argumentsValue={payload:{inconsistencies:unequal(caseData.tasks.flatMap((task)=>task.input.records))},finding_ids:findings.map((finding)=>finding.id)};}
      }else if(!calls.some((call)=>call.function?.name==='complete_task')){
        name='complete_task';argumentsValue={result_ref:resultFor('submit_finding')?.id};
      }
      contacts.push({received_ms:Date.now(),task,operation:name??'settled',input_sha256:crypto.createHash('sha256').update(JSON.stringify(body)).digest('hex')});
      write('issuer/provider-contacts.json',contacts);
      const toolCalls=name?[{id:`call-${crypto.randomUUID()}`,type:'function',function:{name,arguments:JSON.stringify(argumentsValue)}}]:[];
      if(name==='submit_attempt')toolCalls.push({id:`call-${crypto.randomUUID()}`,type:'function',function:{name:'complete_task',arguments:JSON.stringify({result_ref:resultFor('submit_finding')?.id})}});
      const message=name?{role:'assistant',content:null,tool_calls:toolCalls}:{role:'assistant',content:'The assigned fixture Task is complete.'};
      jsonResponse(response,{id:`fixture-${crypto.randomUUID()}`,object:'chat.completion',created:Math.floor(Date.now()/1000),model:'m1',choices:[{index:0,message,finish_reason:name?'tool_calls':'stop'}],usage:{prompt_tokens:4,completion_tokens:3,total_tokens:7}});
    });
  });
  issuer.listen(19443,'127.0.0.1');provider.listen(19447,'127.0.0.1');
  const close=()=>{issuer.close();provider.close();};process.on('SIGTERM',close);process.on('SIGINT',close);
  process.stdout.write(JSON.stringify({fixture_services:'ready',scope:manifest.scope})+'\n');
} else {
  const ca=fs.readFileSync(path.join(root,'issuer/operator-ca.pem'));
  const jwtKey=crypto.createPrivateKey(fs.readFileSync(path.join(root,'issuer/jwt.key.pem')));
  const b64=(value)=>Buffer.from(typeof value==='string'?value:JSON.stringify(value)).toString('base64url');
  const jwt=(subject,nonce)=>{
    const now=Math.floor(Date.now()/1000);
    const header=b64({alg:'EdDSA',typ:'JWT',kid:'test-key'});
    const claims=b64({sub:subject,iss:oidc.issuer,aud:oidc.audience,exp:now+900,nbf:now-1,iat:now,auth_time:now,nonce,principal_type:'human',amr:['webauthn'],acr:'phishing-resistant',groups:[]});
    const input=`${header}.${claims}`;return `${input}.${crypto.sign(null,Buffer.from(input),jwtKey).toString('base64url')}`;
  };
  const request=(pathname,body,auth)=>new Promise((resolve,reject)=>{
    const bytes=Buffer.from(JSON.stringify(body));
    const outgoing=https.request({hostname:'localhost',port:19444,path:pathname,method:'POST',ca,minVersion:'TLSv1.3',headers:{'Content-Type':'application/json','Content-Length':bytes.length,...(auth?{'Authorization':`Bearer ${auth.token}`,'X-Swarm-Session':auth.session_id}:{})},timeout:65000},(response)=>{
      const chunks=[];let size=0;
      response.on('data',(chunk)=>{size+=chunk.length;if(size>32*1048576)response.destroy(new Error('bounded integration read exceeded'));else chunks.push(chunk);});
      response.on('error',reject);response.on('end',()=>{let value;const bytes=Buffer.concat(chunks);try{value=JSON.parse(bytes);if(value&&typeof value==='object')rawPackets.set(value,bytes);}catch{reject(new Error('invalid integration response'));return;}
        if(response.statusCode!==200)reject(new Error(`operator ${pathname} status ${response.statusCode}: ${JSON.stringify(value)}`));else resolve(value);});
    });outgoing.on('error',reject);outgoing.on('timeout',()=>outgoing.destroy(new Error('operator timeout')));outgoing.end(bytes);
  });
  let auth;
  for(let i=0;i<120;i++){
    try{const challenge=await request('/operator/challenge',{});const token=jwt('fixture-admin-a',challenge.nonce);const session=await request('/operator/session',{nonce:challenge.nonce,token});auth={token,session_id:session.session_id};break;}
    catch(error){if(i===0||i%30===0)process.stderr.write(`fixture Gateway readiness: ${error.message}\n`);await new Promise((resolve)=>setTimeout(resolve,1000));}
  }
  if(!auth)throw new Error('normal operator Gateway readiness failed');
  const command=(operation)=>request('/operator/command',{request_id:`fixture-${crypto.randomUUID()}`,operation},auth);
  const initial=await command({command:'read_state',scope:{scope:'global'}});
  if(mode==='inspect'){write('issuer/latest-readback.json',initial);process.exit(0);}
  write('issuer/initial-readback.json',initial);
  const binary=manifest.runtime_binary;
  const expected=crypto.createHash('sha256').update(fs.readFileSync(binary)).digest('hex');
  const attestation=initial.snapshot?.runtime_instances?.n1;
  if(!attestation || Buffer.from(attestation.binary_sha256).toString('hex')!==expected || Buffer.from(attestation.config_sha256).toString('hex')!==manifest.configuration_sha256)throw new Error('runtime measurement differs from frozen fixture before admission');
  const ownedUnits = ()=>{
    const code=`import json,pathlib,subprocess,sys,time\nroot=pathlib.Path(sys.argv[1]);out=[]\nfor p in root.glob('*/process-plan.json'):\n d=json.loads(p.read_text());u=d['unit'];assert u.startswith('solos-agent-') and u.endswith('.service')\n observed=time.monotonic_ns()//1000\n r=subprocess.run(['systemctl','--user','show',u,'--property=ActiveState','--property=SubState','--property=MainPID','--property=ExecMainPID','--property=ExecMainStartTimestampMonotonic','--property=ActiveExitTimestampMonotonic','--property=ControlGroup'],capture_output=True,text=True)\n x=dict(line.split('=',1)for line in r.stdout.splitlines()if '=' in line);c=x.get('ControlGroup','');populated=None\n if c:\n  events=pathlib.Path('/sys/fs/cgroup'+c+'/cgroup.events')\n  if events.exists():populated=dict(line.split()for line in events.read_text().splitlines()).get('populated')\n out.append({'workspace':p.parent.name,'unit':u,'active':x.get('ActiveState','unknown'),'sub':x.get('SubState','unknown'),'pid':int(x.get('MainPID')or 0),'exec_main_pid':int(x.get('ExecMainPID')or 0),'started_mono_us':int(x.get('ExecMainStartTimestampMonotonic')or 0),'exited_mono_us':int(x.get('ActiveExitTimestampMonotonic')or 0),'populated':populated,'observed_mono_us':observed})\nprint(json.dumps(out))`;
    return JSON.parse(execFileSync('sudo',['-u','swarm-lab-cp','env','XDG_RUNTIME_DIR=/run/user/999','DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/999/bus','python3','-c',code,path.join(root,'cp/workers')],{encoding:'utf8',stdio:['ignore','pipe','pipe'],timeout:10000,maxBuffer:1048576}));
  };
  let overlap;
  const observeOverlap=()=>{if(overlap)return;try{const units=ownedUnits();const running=units.filter((unit)=>unit.active==='active'&&unit.pid>0&&unit.populated==='1');if(running.length===10){const start=Math.max(...running.map((unit)=>unit.started_mono_us));const end=Math.min(...running.map((unit)=>unit.observed_mono_us));if(start>0&&end>start)overlap={observed_ms:Date.now(),count:10,window_mono_us:[start,end],minimum_overlap_us:end-start,units:running};}}catch{/* Keep the immutable workload/effect records; absence never earns a concurrency PASS. */}};
  const overlapTimer=setInterval(observeOverlap,250);overlapTimer.unref();
  for(const operation of readJson('issuer/commands.json')) {
    try {await command(operation);}
    catch(error){write('issuer/setup-denial-readback.json',await command({command:'read_state',scope:{scope:'global'}}));throw new Error(`normal setup ${operation.command} denied; protected denial readback retained`,{cause:error});}
  }
  const admitted=await command({command:'read_state',scope:{scope:'global'}});write('issuer/admitted-readback.json',admitted);
  const canonical = (value, numericPath=[]) => {
    if(Array.isArray(value))return `[${value.map((entry,i)=>canonical(entry,[...numericPath,String(i)])).join(',')}]`;
    if(value && typeof value==='object')return `{${Object.keys(value).sort().map((key)=>`${JSON.stringify(key)}:${canonical(value[key],[...numericPath,key])}`).join(',')}}`;
    return JSON.stringify(value);
  };
  const verifyRawSignature = (bytes,domain,trusted,label)=>{
    if(!bytes)return false;const input=path.join(root,`issuer/signature-${label}.json`);const key=path.join(root,`issuer/trusted-${label}.json`);
    fs.writeFileSync(input,bytes,{mode:0o600});fs.writeFileSync(key,JSON.stringify(trusted),{mode:0o600});
    try{execFileSync(binary,['mvp','verify-signature','--input',input,'--trusted-key',key,'--domain',domain],{stdio:['ignore','pipe','pipe'],timeout:30000});return true;}catch{return false;}
  };
  const publicCase=readJson('issuer/public-case.json');
  let final;let finalObservedMs;const deadline=Date.now()+(manifest.actual_local?600000:240000);
  while(Date.now()<deadline){
    await new Promise((resolve)=>setTimeout(resolve,finalObservedMs===undefined?3000:250));
    const packet=await command({command:'read_state',scope:{scope:'global'}});write('issuer/latest-readback.json',packet);const state=packet.snapshot;
    observeOverlap();
    const completed=Object.values(state.domain.tasks).filter((task)=>task.goal_id==='g'&&task.spec.id.startsWith('task-')&&task.state==='completed').length;
    const finalVerdict=state.final_evaluations?.['g/1'];
    if(completed===10 && finalVerdict){final=packet;break;}
    if(finalVerdict)finalObservedMs??=Date.now();
    // Observe the bounded completion drain; this read-only wait grants no new lease
    // or search authority and a missing tenth completion still fails the grade.
    if((finalObservedMs!==undefined&&Date.now()-finalObservedMs>=5000) || Object.values(state.domain.tasks).some((task)=>task.goal_id==='g'&&task.state==='failed')){final=packet;break;}
  }
  if(!final)final=await command({command:'read_state',scope:{scope:'global'}});
  clearInterval(overlapTimer);
  write('issuer/final-readback.json',final);fs.writeFileSync(path.join(root,'issuer/snapshot.json'),rawMember(final,'snapshot'),{mode:0o600});fs.writeFileSync(path.join(root,'issuer/head.json'),rawMember(final,'head'),{mode:0o600});write('issuer/trusted-cp.json',final.snapshot.security.policy.payload.permit_signer);
  const beforeStop=final.snapshot;
  const contacts = manifest.actual_local
    ? Object.values(beforeStop.effects).filter((effect)=>effect.request?.action?.operation==='inference' && effect.result).map((effect)=>({task:effect.task_context?.id}))
    : fs.existsSync(path.join(root,'issuer/provider-contacts.json'))?readJson('issuer/provider-contacts.json'):[];
  const workloadGoals=Object.values(beforeStop.security.grants).filter((grant)=>grant.holder.goal==='g'&&grant.holder.agent.startsWith('agent-'));
  const tasks=Object.values(beforeStop.domain.tasks).filter((task)=>task.goal_id==='g'&&task.spec.id.startsWith('task-'));
  const contributions=tasks.map((task)=>{
    const finding=beforeStop.domain.findings[task.result_ref];const index=Number(task.spec.id.slice(-2));
    let proof;try{proof=JSON.parse(finding?.provenance?.signature??'null');}catch{proof=null;}
    const input=(proof?.payload?.inputs??[]).map((id)=>beforeStop.security.inputs[id]).find((record)=>record?.source_type==='task_input'&&record.id.startsWith(`task-input/g/${task.spec.id}/`));
    const expected=unequal(publicCase.tasks[index].input.records).sort((a,b)=>a.id.localeCompare(b.id));
    const observed=[...(finding?.body?.inconsistencies??[])].sort((a,b)=>a.id.localeCompare(b.id));
    const useful=task.state==='completed'&&finding?.goal_id==='g'&&finding?.goal_version===1&&finding?.task_id===task.spec.id&&finding?.provenance.producer_id===task.claimed_by
      && canonical(observed)===canonical(expected)&&verifyRawSignature(finding?.provenance?.signature?Buffer.from(finding.provenance.signature):null,'finding-provenance',beforeStop.security.policy.payload.provenance_signer,`finding-${index}`)
      && proof.payload.producer===task.claimed_by&&Buffer.from(proof.payload.result_hash).toString('hex')===crypto.createHash('sha256').update(rawPath(final,['snapshot','domain','findings',finding.id,'body'])).digest('hex')
      && input&&Buffer.from(input.hash).toString('hex')===crypto.createHash('sha256').update(rawPath(final,['snapshot','domain','tasks',`g/${task.spec.id}`,'spec','input'])).digest('hex');
    return {task_id:task.spec.id,completed:task.state==='completed',finding_id:finding?.id??null,contribution_rows:observed.length,body_sha256:finding?crypto.createHash('sha256').update(rawPath(final,['snapshot','domain','findings',finding.id,'body'])).digest('hex'):null,lineage_verified:!!useful,recorded_ms:finding?.recorded_ms??null};
  });
  const held=beforeStop.final_evaluations['g/1'];const spec=beforeStop.domain.goals.g.spec;
  const expectedContract={goal_version:spec.version,need_versions:Object.fromEntries(Object.values(beforeStop.domain.needs).filter((need)=>need.goal_id==='g').map((need)=>[need.spec.id,need.spec.version])),rubric_version:spec.rubric_version,evaluation_set_version:spec.evaluation_set_version,evaluator_version:spec.evaluator_version};
  const rawFinal=held?rawPath(final,['snapshot','final_evaluations','g/1']):null;
  const finalSignatureVerified=verifyRawSignature(rawFinal,'protected-evaluation-v1',manifest.evaluator_key,'final-evaluation');
  const finalVerified=held?.payload?.verdict==='PASS'&&held.payload.phase==='held_out'&&held.payload.goal_id==='g'&&held.payload.evaluator_id===spec.evaluator_id
    && canonical(held.payload.contract)===canonical(expectedContract)&&finalSignatureVerified;
  const reservations=Object.values(beforeStop.ledger.intents).filter((reservation)=>reservation.request.goal_id==='g');
  const settled=reservations.filter((reservation)=>reservation.state==='settled');
  const onceReceipts=settled.every((reservation)=>reservation.receipt?.intent_id===reservation.request.intent_id&&Buffer.from(reservation.receipt?.request_hash??[]).toString('hex')===Buffer.from(reservation.request.request_hash).toString('hex')&&!reservation.overrun)
    &&new Set(settled.map((reservation)=>reservation.receipt?.id)).size===settled.length;
  const liabilities=reservations.filter((reservation)=>['reserved','dispatched','unknown'].includes(reservation.state)).reduce((sum,reservation)=>sum+reservation.hold,0);
  const qualification={scope:manifest.scope,production_oidc:'PENDING',live_inference_release:manifest.actual_local?'ACTUAL_LOCAL_FUNCTIONAL_ONLY':'NOT_QUALIFIED',physical_host_independence:'PENDING',binary_sha256:expected,configuration_sha256:manifest.configuration_sha256,
    source_bundle_sha256:attestation.bundle_sha256?Buffer.from(attestation.bundle_sha256).toString('hex'):null,controller_sha256:crypto.createHash('sha256').update(fs.readFileSync(new URL(import.meta.url))).digest('hex'),
    distinct_pi_workloads:new Set(workloadGoals.map((grant)=>grant.holder.agent)).size,distinct_model_tasks:new Set(contacts.map((contact)=>contact.task)).size,
    findings:Object.values(beforeStop.domain.findings).filter((finding)=>finding.goal_id==='g').length,tasks_completed:tasks.filter((task)=>task.state==='completed').length,
    useful_contributors:contributions.filter((entry)=>entry.lineage_verified).length,contributions,temporal_overlap:overlap??{count:0},
    ledger_spent:beforeStop.ledger.accounts.global.total.spent,settled_receipts:settled.length,once_receipts_verified:onceReceipts,outstanding_liability:liabilities,
    held_out:held?.payload?.verdict??null,independent_signature_verified:finalSignatureVerified,independent_final_verified:!!finalVerified,goal_status:beforeStop.domain.goals.g.status,
    final_signature:held?{raw_sha256:crypto.createHash('sha256').update(rawFinal).digest('hex'),trusted_key:manifest.evaluator_key,signature:held.signature,request_id:held.payload.request_id,candidate_hash:held.payload.candidate_hash,contract:held.payload.contract,protected_set_digest:held.payload.protected_set_digest}:null,
    board_entries:Object.values(beforeStop.domain.boards??{}).reduce((sum,board)=>sum+(Array.isArray(board)?board.length:board.entries?.length??0),0),needs:Object.values(beforeStop.domain.needs).filter((need)=>need.goal_id==='g').map((need)=>({id:need.spec.id,version:need.spec.version,satisfied:need.satisfied})),
    runtime_functional_verdict:'INCONCLUSIVE',cleanup_verdict:'PENDING'};
  const countsPass=qualification.distinct_pi_workloads===10&&qualification.distinct_model_tasks===10&&qualification.useful_contributors===10&&qualification.tasks_completed===10;
  qualification.scripted_counts_verdict=countsPass?'PASS':'FAIL';
  try{execFileSync(binary,['mvp','verify','--snapshot',path.join(root,'issuer/snapshot.json'),'--head',path.join(root,'issuer/head.json'),'--trusted-key',path.join(root,'issuer/trusted-cp.json'),'--output',path.join(root,'issuer/evidence-verifier.json')],{stdio:['ignore','pipe','pipe'],timeout:60000});qualification.evidence_verdict='PASS';}
  catch{qualification.evidence_verdict='FAIL';}
  qualification.readback_sha256=crypto.createHash('sha256').update(fs.readFileSync(path.join(root,'issuer/final-readback.json'))).digest('hex');
  write('issuer/integration-report.json',qualification);
  const challenge=await request('/operator/challenge',{});const token=jwt('fixture-admin-a',challenge.nonce);const session=await request('/operator/session',{nonce:challenge.nonce,token});auth={token,session_id:session.session_id};
  await command({command:'engage_stop',scope:{scope:'goal',tenant:'t',goal:'g'},reason:'isolated integration completed; drain owned units'});
  for(let i=0;i<20;i++){
    await new Promise((resolve)=>setTimeout(resolve,1000));
    const stopped=await command({command:'read_state',scope:{scope:'global'}});
    const live=Object.values(stopped.snapshot.security.grants).filter((grant)=>grant.holder.goal==='g'&&grant.holder.agent.startsWith('agent-')&&!grant.revoked);
    const units=ownedUnits();const populated=units.some((unit)=>unit.active==='active'||unit.pid>0||unit.populated==='1');
    const unresolved=Object.values(stopped.snapshot.ledger.intents).filter((reservation)=>reservation.request.goal_id==='g'&&['reserved','dispatched','unknown'].includes(reservation.state)).reduce((sum,reservation)=>sum+reservation.hold,0);
    if(live.length===0&&!populated&&units.length>=10&&unresolved===0){qualification.cleanup_verdict='PASS';qualification.owned_units=units;qualification.outstanding_liability_after_stop=unresolved;write('issuer/stopped-readback.json',stopped);break;}
  }
  qualification.runtime_functional_verdict=countsPass&&finalVerified&&qualification.goal_status==='completed'&&onceReceipts&&liabilities===0&&settled.length>0&&overlap?.count===10&&qualification.evidence_verdict==='PASS'&&qualification.cleanup_verdict==='PASS'?'PASS':'FAIL';
  write('issuer/integration-report.json',qualification);process.stdout.write(JSON.stringify(qualification)+'\n');
  if(qualification.runtime_functional_verdict!=='PASS')process.exitCode=1;
}
