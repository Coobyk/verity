package sh.coobyk.verity

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import sh.coobyk.verity.ui.AppViewModel
import sh.coobyk.verity.ui.VerityApp
import sh.coobyk.verity.ui.VerityTheme

class MainActivity : ComponentActivity() {

    private val vm: AppViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            VerityTheme {
                VerityApp(vm)
            }
        }
    }
}
