package sh.coobyk.verity.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle

@Composable
fun VerityApp(vm: AppViewModel) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    val conv by vm.conversation.collectAsStateWithLifecycle()
    val settingsOpen by vm.showSettings.collectAsStateWithLifecycle()
    val snackbar = remember { SnackbarHostState() }

    // Polling only runs while the app is resumed.
    LifecycleResumeEffect(Unit) {
        vm.setForeground(true)
        onPauseOrDispose { vm.setForeground(false) }
    }

    LaunchedEffect(ui.status) {
        val msg = ui.status
        if (msg != null) {
            snackbar.showSnackbar(msg)
            vm.clearStatus()
        }
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(VerityBlack)
            .statusBarsPadding()
            .navigationBarsPadding(),
    ) {
        val c = conv
        when {
            settingsOpen -> SettingsScreen(vm)
            c != null -> ChatScreen(vm, c)
            else -> ListScreen(vm)
        }
        SnackbarHost(
            hostState = snackbar,
            modifier = Modifier.align(Alignment.BottomCenter),
        )
    }
}
