export function setupPage({ complete, nonce }) {
  return `<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Set up Graft</title>
<style nonce="${nonce}">
:root{color-scheme:light dark;font:15px/1.55 system-ui,sans-serif;background:light-dark(#fafafa,#171717);color:light-dark(#222,#eee)}*{box-sizing:border-box}body{margin:0}main{max-width:560px;margin:64px auto;padding:0 24px 48px}header{display:flex;justify-content:space-between;align-items:center;border-bottom:1px solid #8884;padding-bottom:16px}h1{font-size:26px;line-height:1.3;font-weight:600;margin:30px 0 12px}h2{font-size:18px}p{margin:12px 0;color:light-dark(#555,#bbb)}label{display:block;margin:18px 0 6px}input,select,button{font:inherit;border:1px solid #8886;border-radius:6px;padding:9px 12px;background:light-dark(#fff,#222);color:inherit}input,select{width:100%}input[type=checkbox]{width:auto}button{cursor:pointer}button:disabled{opacity:.55;cursor:wait}button.primary{background:light-dark(#222,#eee);color:light-dark(#fff,#222);border-color:transparent}input:focus-visible,button:focus-visible,select:focus-visible{outline:2px solid #6c8ccb;outline-offset:2px}.actions{display:flex;gap:10px;margin-top:26px;flex-wrap:wrap}small{display:block;color:light-dark(#666,#aaa);margin-top:6px}#message{white-space:pre-wrap}#message.error{color:light-dark(#a22,#ffaaaa)}[hidden]{display:none!important}code{overflow-wrap:anywhere}fieldset{border:0;padding:0;margin:0}#language{width:auto;font-size:13px}footer{margin-top:32px;border-top:1px solid #8884;padding-top:16px} @media(max-width:600px){main{margin-top:24px}}
pre{white-space:pre-wrap;overflow-wrap:anywhere;padding:14px;border:1px solid #8884;border-radius:6px}button:disabled{cursor:not-allowed}form[aria-busy=true] button:disabled{cursor:wait}
</style><main><header><strong>Graft</strong><select id="language" aria-label="Language"><option value="en">English</option><option value="zh">简体中文</option></select></header>
<h1 id="heading"></h1><p id="intro"></p>
<section id="entry"><p id="entry-help"></p><pre><code id="command"></code></pre><button type="button" id="copy-command" data-text="copyCommand"></button><p id="entry-next"></p><button type="button" id="retry" hidden data-text="retry"></button></section>
<noscript><p>JavaScript is required to verify the initialization link. / 验证初始化链接需要启用 JavaScript。</p></noscript>
<form id="form" hidden><fieldset id="setup-fields" disabled>
<label for="mode" data-text="storage"></label><select id="mode"><option value="local" data-text="local"></option><option value="s3" data-text="s3"></option></select>
<p id="local-note" data-text="localNote"></p>
<fieldset id="s3-fields" hidden disabled><legend hidden>S3</legend>
<label for="endpoint" data-text="endpoint"></label><input id="endpoint" type="url" placeholder="https://s3.example.com">
<label for="region" data-text="region"></label><input id="region" autocomplete="off">
<label for="bucket" data-text="bucket"></label><input id="bucket" autocomplete="off">
<label for="accessKeyId">Access Key ID</label><input id="accessKeyId" autocomplete="off" spellcheck="false">
<label for="secretAccessKey">Secret Access Key</label><input id="secretAccessKey" type="password" autocomplete="new-password">
<details><summary data-text="advanced"></summary><label for="prefix" data-text="prefix"></label><input id="prefix" value="graft/">
<label><input id="pathStyle" type="checkbox" checked> <span data-text="pathStyle"></span></label>
<label for="sessionToken" data-text="sessionToken"></label><input id="sessionToken" type="password" autocomplete="off"></details>
<small data-text="s3Note"></small></fieldset>
<div class="actions"><button type="button" id="test" hidden data-text="test"></button><button class="primary" id="submit" data-text="finish"></button></div></fieldset></form>
<p id="message" role="status" aria-live="polite"></p>
<section id="result" hidden><label for="address" data-text="address"></label><input id="address" readonly><button type="button" id="copy-address" data-text="copyAddress"></button>
<label for="key" data-text="key"></label><input id="key" readonly spellcheck="false"><button type="button" id="copy-key" data-text="copyKey"></button>
<p data-text="save"></p><h2 data-text="connect"></h2><p data-text="connectHelp"></p></section>
<footer><small data-text="footer"></small></footer></main>
<script nonce="${nonce}">
const strings={en:{heading:'Set up your Graft server',intro:'Choose where to store your data. Graft will create your server access key.',storage:'Storage',local:'Local disk',s3:'S3-compatible storage',localNote:'Use the persistent Docker data volume. No additional configuration is needed.',endpoint:'S3 endpoint',region:'Region',bucket:'Bucket name',advanced:'Advanced settings',prefix:'Dedicated object prefix',pathStyle:'Use path-style addressing',sessionToken:'Session token (optional)',s3Note:'Use an existing private bucket and a dedicated prefix. Setup checks reads, writes and deletion with a temporary object. Server metadata still requires the Docker data volume.',test:'Test connection',finish:'Complete setup',checking:'Checking storage…',finishing:'Checking storage and setting up…',tested:'Connection successful. You can complete setup.',done:'Your server is ready',doneIntro:'Save your connection details before closing this page.',already:'Setup is complete',alreadyIntro:'Connect Lite using your saved access key. Setup cannot be run again.',missing:'Open the initialization link from your server terminal. Run: docker compose exec graft node services/graft-remote-docker/manage.mjs setup-link',address:'Server address',key:'Access key',copyAddress:'Copy address',copyKey:'Copy access key',copied:'Copied.',copyFailed:'Select the field and copy its contents manually.',save:'Save the access key in your password manager. This page shows it only once. It grants access to every Space on this server.',connect:'Connect Eidos Lite',connectHelp:'Open Settings → Sync → Add server. Paste the server address and access key, then test and save. Your Space sync password is separate; keep it safe too.',footer:'Keep the Docker data volume and configuration when restarting or upgrading.',network:'Could not reach the server. Retry the check. If setup completed but this response was lost, use the server access-key command.'},zh:{heading:'设置你的 Graft 服务器',intro:'选择数据存储位置，Graft 会自动创建服务器访问密钥。',storage:'存储方式',local:'本地磁盘',s3:'S3 兼容存储',localNote:'使用 Docker 持久化数据卷，无需额外配置。',endpoint:'S3 端点',region:'区域',bucket:'存储桶名称',advanced:'高级设置',prefix:'独占对象前缀',pathStyle:'使用路径式寻址',sessionToken:'会话令牌（可选）',s3Note:'使用已有私有桶和独占前缀。设置时会用临时对象检查读写和删除。服务器元数据仍需保存在 Docker 数据卷中。',test:'测试连接',finish:'完成设置',checking:'正在检查存储…',finishing:'正在检查存储并完成设置…',tested:'连接成功，可以完成设置。',done:'服务器已就绪',doneIntro:'关闭页面前，请保存连接信息。',already:'设置已完成',alreadyIntro:'使用已保存的访问密钥连接 Lite，无需再次初始化。',missing:'请打开服务器终端提供的初始化链接。运行：docker compose exec graft node services/graft-remote-docker/manage.mjs setup-link',address:'服务器地址',key:'访问密钥',copyAddress:'复制地址',copyKey:'复制访问密钥',copied:'已复制。',copyFailed:'请选中字段内容并手动复制。',save:'请将访问密钥保存到密码管理器。此页面仅展示一次，它可以访问这台服务器上的所有 Space。',connect:'连接 Eidos Lite',connectHelp:'打开设置 → 同步 → 添加服务器，粘贴服务器地址和访问密钥，测试并保存。Space 同步密码与访问密钥不同，也需要妥善保存。',footer:'重启和升级时，请保留 Docker 数据卷及配置。',network:'无法连接服务器，请重试检查。如果设置已完成但响应丢失，可使用服务器 access-key 命令取回密钥。'}};
Object.assign(strings.en,{missingHeading:'Open your initialization link',missingIntro:'This page has not been authorized to set up the server. Get the private link before entering storage details.',invalidHeading:'This initialization link is not valid',invalidIntro:'The server did not accept this link. It may be incomplete or belong to another installation. Retrieve the link again.',verifyingHeading:'Verifying initialization link…',verifyingIntro:'Storage details will appear after the server accepts the link.',originHeading:'Open setup at the configured address',originIntro:'Open the complete link directly at GRAFT_PUBLIC_ORIGIN. Check your proxy for a redirect to a different origin.',unreachableHeading:'Could not verify the link',unreachableIntro:'Check your connection and HTTPS proxy, then retry verification. Storage details stay hidden until verification succeeds.',runOnServer:'On your server, in the directory containing your Compose file, run:',openFullLink:'Open the complete URL printed by the command, including #token=… . Keep it private. The server root address and /setup alone do not authorize setup.',recoverHelp:'If you did not save your access key, retrieve it on your server from the Compose directory:',recoverNext:'Use the server address and access key in Lite → Sync → Add server. Restarting does not reopen setup.',copyCommand:'Copy command',retry:'Retry verification'});
Object.assign(strings.zh,{missingHeading:'请打开初始化链接',missingIntro:'此页面尚未获得服务器初始化授权。请先获取私有链接，再填写存储信息。',invalidHeading:'初始化链接无效',invalidIntro:'服务器未接受此链接。链接可能不完整，或属于另一套安装，请重新取回链接。',verifyingHeading:'正在验证初始化链接…',verifyingIntro:'服务器接受链接后，才会显示存储配置。',originHeading:'请从配置的服务器地址打开',originIntro:'请直接打开 GRAFT_PUBLIC_ORIGIN 下的完整链接，检查代理是否跳转到了其他来源。',unreachableHeading:'无法验证初始化链接',unreachableIntro:'请检查网络和 HTTPS 代理，再重试验证。验证成功前，存储配置保持隐藏。',runOnServer:'在服务器上进入 Compose 文件所在目录，执行：',openFullLink:'打开命令输出的完整网址，包括 #token=… 部分，请勿分享。服务器根地址或单独的 /setup 均不具备初始化授权。',recoverHelp:'如果没有保存访问密钥，请在服务器的 Compose 目录中执行以下命令取回：',recoverNext:'在 Lite → 同步 → 添加服务器中填写服务器地址和访问密钥。重启不会重新开放初始化。',copyCommand:'复制命令',retry:'重新验证'});
const $=id=>document.getElementById(id);let lang=navigator.language.startsWith('zh')?'zh':'en';let busy=false;let token=new URLSearchParams(location.hash.slice(1)).get('token')||'';
let state=${complete}?'already':token?'verifying':'missing';
history.replaceState(null,'',location.pathname);const t=k=>strings[lang][k];
function message(text,error=false){$('message').textContent=text;$('message').className=error?'error':'';}
function render(){
  document.documentElement.lang=lang==='zh'?'zh-CN':'en';$('language').value=lang;
  document.querySelectorAll('[data-text]').forEach(el=>el.textContent=t(el.dataset.text));
  const authorized=state==='authorized';const done=state==='done';const already=state==='already';
  $('heading').textContent=t(authorized?'heading':done?'done':already?'already':state+'Heading');
  $('intro').textContent=t(authorized?'intro':done?'doneIntro':already?'alreadyIntro':state+'Intro');
  $('form').hidden=!authorized;$('setup-fields').disabled=!authorized||busy;
  $('form').setAttribute('aria-busy',String(busy));
  $('entry').hidden=authorized||done||state==='verifying';
  $('entry-help').textContent=t(already?'recoverHelp':'runOnServer');
  $('command').textContent='docker compose exec graft node services/graft-remote-docker/manage.mjs '+(already?'access-key':'setup-link');
  $('entry-next').textContent=t(already?'recoverNext':'openFullLink');
  $('retry').hidden=state!=='unreachable';
  $('submit').disabled=!authorized||busy;$('test').disabled=!authorized||busy;
}
function rejectAuthorization(status){
  state=status===409?'already':status===403?'origin':status===401?'invalid':'unreachable';
  if(state!=='unreachable'){token='';$('form').reset();}
  message('');render();
}
async function verify(){
  state='verifying';message('');render();
  try{
    const response=await fetch('/setup/status',{method:'POST',headers:{Authorization:'Bearer '+token}});
    if(!response.ok){rejectAuthorization(response.status);return;}
    const result=await response.json();
    if(result.status!=='authorized'){rejectAuthorization(503);return;}
    state='authorized';render();
  }catch{rejectAuthorization(503);}
}
$('language').onchange=()=>{lang=$('language').value;render();};
$('mode').onchange=()=>{if(state!=='authorized')return;const s3=$('mode').value==='s3';$('s3-fields').hidden=!s3;$('s3-fields').disabled=!s3;$('test').hidden=!s3;$('local-note').hidden=s3;['endpoint','region','bucket','accessKeyId','secretAccessKey'].forEach(id=>$(id).required=s3);message('');};
function input(){const v={mode:$('mode').value,pathStyle:$('pathStyle').checked};['endpoint','region','bucket','prefix','accessKeyId','secretAccessKey','sessionToken'].forEach(id=>v[id]=$(id).value);return v;}
async function submit(test){if(state!=='authorized'||busy||!$('form').reportValidity())return;const value=input();busy=true;render();message(t(test?'checking':'finishing'));try{const response=await fetch(test?'/setup/test':'/setup',{method:'POST',headers:{'Content-Type':'application/json',Authorization:'Bearer '+token},body:JSON.stringify(value)});const result=await response.json();if(!response.ok){if(response.status===401||response.status===403){rejectAuthorization(response.status);return;}if(response.status===409){await verify();if(state!=='authorized')return;}message(result.error,true);return;}if(test){message(t('tested'));return;}state='done';token='';$('form').reset();$('result').hidden=false;$('address').value=result.origin;$('key').value=result.accessKey;message('');render();$('address').focus();}catch{message(t('network'),true);}finally{busy=false;render();}}
$('form').onsubmit=e=>{e.preventDefault();void submit(false);};$('test').onclick=()=>void submit(true);
for(const [button,field]of[['copy-command','command'],['copy-address','address'],['copy-key','key']])$(button).onclick=async()=>{try{await navigator.clipboard.writeText($(field).value??$(field).textContent);message(t('copied'));}catch{if(field!=='command'){$(field).focus();$(field).select();}message(t('copyFailed'));}};
$('retry').onclick=()=>void verify();
render();if(state==='verifying')void verify();
</script></html>`;
}
