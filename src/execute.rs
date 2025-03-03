use std::os::fd::AsRawFd;

use shellenv::jobs::{ChildProc, JobBldr};

use crate::{builtin::export::export, libsh::{error::Blame, sys::{execvpe, get_bin_path}, utils::{ArgVec, StrOps}}, parse::{lex::Token, parse::{CmdGuard, NdFlag, Node, NdRule, SynTree}}, prelude::*};

pub struct Executor<'a> {
	ast: SynTree,
	shenv: &'a mut ShEnv
}

impl<'a> Executor<'a> {
	pub fn new(ast: SynTree, shenv: &'a mut ShEnv) -> Self {
		Self { ast, shenv }
	}
	pub fn walk(&mut self) -> ShResult<()> {
		log!(DEBUG, "Starting walk");
		while let Some(node) = self.ast.next_node() {
			let span = node.span();
			if let NdRule::CmdList { cmds } = node.clone().into_rule() {
				log!(TRACE, "{:?}", cmds);
				exec_list(cmds, self.shenv).try_blame(span)?
			} else { unreachable!() }
		}
		Ok(())
	}
}

fn exec_list(list: Vec<(Option<CmdGuard>, Node)>, shenv: &mut ShEnv) -> ShResult<()> {
	log!(DEBUG, "Executing list");
	let mut list = VecDeque::from(list);
	while let Some(cmd_info) = list.fpop() {
		let guard = cmd_info.0;
		let cmd = cmd_info.1;
		let span = cmd.span();

		if let Some(guard) = guard {
			let code = shenv.get_code();
			match guard {
				CmdGuard::And => {
					if code != 0 { break; }
				}
				CmdGuard::Or => {
					if code == 0 { break; }
				}
			}
		}
		log!(TRACE, "{:?}", *cmd.rule());
		match *cmd.rule() {
			NdRule::Command {..} => dispatch_command(cmd, shenv).try_blame(span)?,
			NdRule::Subshell {..} => exec_subshell(cmd,shenv).try_blame(span)?,
			NdRule::FuncDef {..} => exec_funcdef(cmd,shenv).try_blame(span)?,
			NdRule::Assignment {..} => exec_assignment(cmd,shenv).try_blame(span)?,
			NdRule::Pipeline {..} => exec_pipeline(cmd, shenv).try_blame(span)?,
			_ => unimplemented!()
		}
	}
	Ok(())
}

fn dispatch_command(mut node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	let mut is_builtin = false;
	let mut is_func = false;
	let mut is_subsh = false;
	if let NdRule::Command { ref mut argv, redirs: _ } = node.rule_mut() {
		*argv = expand_argv(argv.to_vec(), shenv);
		let cmd = argv.first().unwrap().to_string();
		if shenv.logic().get_function(&cmd).is_some() {
			is_func = true;
		} else if node.flags().contains(NdFlag::BUILTIN) {
			is_builtin = true;
		}
	} else if let NdRule::Subshell { body: _, ref mut argv, redirs: _ } = node.rule_mut() {
		*argv = expand_argv(argv.to_vec(), shenv);
		is_subsh = true;
	} else { unreachable!() }

	if is_builtin {
		exec_builtin(node, shenv)?;
	} else if is_func {
		exec_func(node, shenv)?;
	} else if is_subsh {
		exec_subshell(node, shenv)?;
	} else {
		exec_cmd(node, shenv)?;
	}
	Ok(())
}

fn exec_func(node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	let rule = node.into_rule();
	if let NdRule::Command { argv, redirs } = rule {
		let mut argv_iter = argv.into_iter();
		let func_name = argv_iter.next().unwrap().to_string();
		let body = shenv.logic().get_function(&func_name).unwrap().to_string();
		let snapshot = shenv.clone();
		shenv.vars_mut().reset_params();
		while let Some(arg) = argv_iter.next() {
			shenv.vars_mut().bpush_arg(&arg.to_string());
		}
		shenv.collect_redirs(redirs);

		let lex_input = Rc::new(body);
		let tokens = Lexer::new(lex_input).lex();
		match Parser::new(tokens).parse() {
			Ok(syn_tree) => {
				match Executor::new(syn_tree, shenv).walk() {
					Ok(_) => { /* yippee */ }
					Err(e) => {
						*shenv = snapshot;
						return Err(e.into())
					}
				}
			}
			Err(e) => {
				*shenv = snapshot;
				return Err(e.into())
			}
		}
		*shenv = snapshot;
	}
	Ok(())
}

fn exec_funcdef(node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	let rule = node.into_rule();
	if let NdRule::FuncDef { name, body } = rule {
		let name_raw = name.to_string();
		let name = name_raw.trim_end_matches("()");
		let body_raw = body.to_string();
		let body = body_raw[1..body_raw.len() - 1].trim();

		shenv.logic_mut().set_function(name, body);
	} else { unreachable!() }
	Ok(())
}

fn exec_subshell(node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	let snapshot = shenv.clone();
	shenv.vars_mut().reset_params();
	let rule = node.into_rule();
	if let NdRule::Subshell { body, argv, redirs } = rule {
		if shenv.ctx().flags().contains(ExecFlags::NO_FORK) {
			shenv.ctx_mut().unset_flag(ExecFlags::NO_FORK); // Allow sub-forks in this case
			shenv.collect_redirs(redirs);
			if let Err(e) = shenv.ctx_mut().activate_rdrs() {
				write_err(e)?;
				exit(1);
			}
			for arg in argv {
				shenv.vars_mut().bpush_arg(&arg.to_string());
			}
			let body_raw = body.to_string();
			let lexer_input = Rc::new(
					body_raw[1..body_raw.len() - 1].to_string()
			);
			let token_stream = Lexer::new(lexer_input).lex();
			match Parser::new(token_stream).parse() {
				Ok(syn_tree) => {
					if let Err(e) = Executor::new(syn_tree, shenv).walk() {
						write_err(e)?;
						exit(1);
					}
				}
				Err(e) => {
					write_err(e)?;
					exit(1);
				}
			}
			exit(0);
		} else {
			match unsafe { fork()? } {
				Child => {
					shenv.collect_redirs(redirs);
					if let Err(e) = shenv.ctx_mut().activate_rdrs() {
						write_err(e)?;
						exit(1);
					}
					for arg in argv {
						shenv.vars_mut().bpush_arg(&arg.to_string());
					}
					let body_raw = body.to_string();
					let lexer_input = Rc::new(
							body_raw[1..body_raw.len() - 1].to_string()
					);
					let token_stream = Lexer::new(lexer_input).lex();
					match Parser::new(token_stream).parse() {
						Ok(syn_tree) => {
							if let Err(e) = Executor::new(syn_tree, shenv).walk() {
								write_err(e)?;
								exit(1);
							}
						}
						Err(e) => {
							write_err(e)?;
							exit(1);
						}
					}
					exit(0);
				}
				Parent { child } => {
					*shenv = snapshot;
					let children = vec![
						ChildProc::new(child, Some("anonymous subshell"), Some(child))?
					];
					let job = JobBldr::new()
						.with_children(children)
						.with_pgid(child)
						.build();
					wait_fg(job, shenv)?;
				}
			}
		}
	} else { unreachable!() }
	Ok(())
}

fn exec_builtin(node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	log!(DEBUG, "Executing builtin");
	let command = if let NdRule::Command { argv, redirs: _ } = node.rule() {
		argv.first().unwrap().to_string()
	} else { unreachable!() };

	log!(TRACE, "{}", command.as_str());
	match command.as_str() {
		"echo" => echo(node, shenv)?,
		"cd" => cd(node,shenv)?,
		"pwd" => pwd(node, shenv)?,
		"export" => export(node, shenv)?,
		"jobs" => jobs(node, shenv)?,
		"fg" => continue_job(node, shenv, true)?,
		"bg" => continue_job(node, shenv, false)?,
		"read" => read_builtin(node, shenv)?,
		"alias" => alias(node, shenv)?,
		_ => unimplemented!("Have not yet implemented support for builtin `{}'",command)
	}
	log!(TRACE, "done");
	Ok(())
}

fn exec_assignment(node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	log!(DEBUG, "Executing assignment");
	let rule = node.into_rule();
	if let NdRule::Assignment { assignments, cmd } = rule {
		log!(TRACE, "Assignments: {:?}", assignments);
		log!(TRACE, "Command: {:?}", cmd);
		let mut assigns = assignments.into_iter();
		if let Some(cmd) = cmd {
			while let Some(assign) = assigns.next() {
				let assign_raw = assign.to_string();
				if let Some((var,val)) = assign_raw.split_once('=') {
					shenv.vars_mut().export(var, val);
				}
			}
			if cmd.flags().contains(NdFlag::BUILTIN) {
				exec_builtin(*cmd, shenv)?;
			} else {
				exec_cmd(*cmd, shenv)?;
			}
		} else {
			while let Some(assign) = assigns.next() {
				let assign_raw = assign.to_string();
				if let Some((var,val)) = assign_raw.split_once('=') {
					shenv.vars_mut().set_var(var, val);
				}
			}
		}
	} else { unreachable!() }
	Ok(())
}

fn exec_pipeline(node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	log!(DEBUG, "Executing pipeline");
	let rule = node.into_rule();
	if let NdRule::Pipeline { cmds } = rule {
		let mut prev_rpipe: Option<i32> = None;
		let mut cmds       = VecDeque::from(cmds);
		let mut pgid       = None;
		let mut cmd_names  = vec![];
		let mut pids       = vec![];

		while let Some(cmd) = cmds.pop_front() {
			let (r_pipe, w_pipe) = if cmds.is_empty() {
				// If we are on the last command, don't make new pipes
				(None,None)
			} else {
				let (r_pipe, w_pipe) = c_pipe()?;
				(Some(r_pipe),Some(w_pipe))
			};
			if let NdRule::Command { argv, redirs: _ } = cmd.rule() {
				let cmd_name = argv.first().unwrap().span().get_slice().to_string();
				cmd_names.push(cmd_name);
			} else if let NdRule::Subshell {..} = cmd.rule() {
				cmd_names.push("subshell".to_string());
			} else { unimplemented!() }

			match unsafe { fork()? } {
				Child => {
					// Set NO_FORK since we are already in a fork, to prevent unnecessarily forking again
					shenv.ctx_mut().set_flag(ExecFlags::NO_FORK);
					// We close this r_pipe since it's the one the next command will use, so not useful here
					if let Some(r_pipe) = r_pipe {
						close(r_pipe.as_raw_fd())?;
					}

					// Create some redirections
					if let Some(w_pipe) = w_pipe {
						let wpipe_redir = Redir::new(1, RedirType::Output, RedirTarget::Fd(w_pipe.as_raw_fd()));
						shenv.ctx_mut().push_rdr(wpipe_redir);
					}
					// Use the r_pipe created in the last iteration
					if let Some(prev_rpipe) = prev_rpipe {
						let rpipe_redir = Redir::new(0, RedirType::Input, RedirTarget::Fd(prev_rpipe.as_raw_fd()));
						shenv.ctx_mut().push_rdr(rpipe_redir);
					}

					dispatch_command(cmd, shenv)?;
					exit(0);
				}
				Parent { child } => {
					// Close the write pipe out here to signal EOF
					if let Some(w_pipe) = w_pipe {
						close(w_pipe.as_raw_fd())?;
					}
					if pgid.is_none() {
						pgid = Some(child);
					}
					pids.push(child);
					prev_rpipe = r_pipe;
				}
			}
		}
		for (i,pid) in pids.iter().enumerate() {
			let command = cmd_names.get(i).unwrap();
			let children = vec![
				ChildProc::new(*pid, Some(&command), pgid)?
			];
			let job = JobBldr::new()
				.with_children(children)
				.with_pgid(pgid.unwrap())
				.build();
			wait_fg(job, shenv)?;
		}
	} else { unreachable!() }
	Ok(())
}

fn exec_cmd(node: Node, shenv: &mut ShEnv) -> ShResult<()> {
	log!(DEBUG, "Executing command");
	let blame = node.span();
	let rule = node.into_rule();

	if let NdRule::Command { argv, redirs } = rule {
		let (argv,envp) = prep_execve(argv, shenv);
		let command = argv.first().unwrap().to_string();
		if get_bin_path(&command, shenv).is_some() {

			shenv.save_io()?;
			if shenv.ctx().flags().contains(ExecFlags::NO_FORK) {
				log!(TRACE, "Not forking");
				shenv.collect_redirs(redirs);
				if let Err(e) = shenv.ctx_mut().activate_rdrs() {
					eprintln!("{:?}",e);
					exit(1);
				}
				if let Err(errno) = execvpe(command, argv, envp) {
					if errno != Errno::EFAULT {
						exit(errno as i32);
					}
				}
			} else {
				log!(TRACE, "Forking");
				match unsafe { fork()? } {
					Child => {
						log!(DEBUG, redirs);
						shenv.collect_redirs(redirs);
						if let Err(e) = shenv.ctx_mut().activate_rdrs() {
							eprintln!("{:?}",e);
							exit(1);
						}
						execvpe(command, argv, envp)?;
						exit(1);
					}
					Parent { child } => {
						let children = vec![
							ChildProc::new(child, Some(&command), Some(child))?
						];
						let job = JobBldr::new()
							.with_children(children)
							.with_pgid(child)
							.build();
						log!(TRACE, "New job: {:?}", job);
						wait_fg(job, shenv)?;
					}
				}
			}
		} else {
			return Err(ShErr::full(ShErrKind::CmdNotFound, format!("{}", command), blame))
		}
	} else { unreachable!("Found this rule in exec_cmd: {:?}", rule) }
	Ok(())
}

fn prep_execve(argv: Vec<Token>, shenv: &mut ShEnv) -> (Vec<String>, Vec<String>) {
	log!(DEBUG, "Preparing execvpe args");
	let argv_s = argv.as_strings(shenv);
	log!(DEBUG, argv_s);

	let mut envp = vec![];
	let env_vars = shenv.vars().env().clone();
	let mut entries = env_vars.iter().collect::<VecDeque<(&String,&String)>>();
	while let Some(entry) = entries.fpop() {
		let key = entry.0;
		let val = entry.1;
		let formatted = format!("{}={}",key,val);
		envp.push(formatted);
	}
	log!(TRACE, argv_s);
	(argv_s, envp)
}
